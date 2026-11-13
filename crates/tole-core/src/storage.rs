//! Storage trait + JSONL session backend (default).
//!
//! One file per session (`<session-id>.jsonl`). The file is a **replay
//! recipe**, not the state: opening replays lines into in-memory maps and all
//! queries run in RAM. Writes append one physical line per commit (an array
//! line groups one atomic transaction); a torn final line is discarded whole.
//!
//! **Redacted at the seam (#596):** every entry payload, and the pinned
//! system prompt, is redacted with the process [`SecretSet`] before its line
//! is written, and the committed in-memory entry is that same redacted
//! form. The provider is fed from these entries, so the log stores exactly
//! what the provider saw and a replay reproduces it. An entry that hid a
//! secret carries `"redacted": ["NAME", ...]` on the wire (names only).
//! Older logs, written raw, load unchanged; nothing is rewritten.
//!
//! See `docs/architecture.md` §4 for the format rationale and the full line
//! grammar. Format versions are bumped only on breaking changes; opening a
//! file written by a **newer** format version is a hard error (refuse to
//! guess), while older versions replay cleanly (this build understands them).
//!
//! # Wire format
//!
//! ```jsonl
//! {"v":1,"kind":"header","id":"<session-id>","storageVersion":1,"createdAt":...,"cwd":"..."}
//! [{"kind":"entry","seq":101,...},{"kind":"register","op":"set","seq":102,...}]
//! {"kind":"usage","seq":110,"id":"u_7","entryId":"e_51","usage":{...}}
//! ```

use std::collections::BTreeMap;
use std::ffi::{OsStr, OsString};
use std::fs::File;
use std::io::{BufRead, BufReader, BufWriter, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::entry::{Entry, EntryType, NewEntry};
use crate::register::{is_valid_namespace, RegisterOp, RegisterWrite};
use crate::safe_dir::{Mode, SafeDir};
use crate::secrets::SecretSet;
use crate::state::{MachineState, Pc, StateTransition};
use crate::STORAGE_VERSION;

/// Errors returned by any [`Storage`] implementation.
#[derive(Debug, thiserror::Error)]
pub enum StorageError {
    /// The file exists but is not a valid session file.
    #[error("corrupt session file: {0}")]
    Corrupt(String),
    /// The file was written by a newer format version this build cannot read.
    #[error("unsupported session format: file is version {file}, this build reads <= {supported}; upgrade the binary to open it")]
    UnsupportedVersion {
        /// Format version found in the file header.
        file: u32,
        /// Highest format version this build understands.
        supported: u32,
    },
    /// A commit attempted to mutate state with a stale CAS token.
    #[error("stale state transition: expected seq {expected}, machine holds {actual}")]
    StaleTransition {
        /// The `expected_seq` the caller presented.
        expected: u64,
        /// The `seq` the machine actually holds.
        actual: u64,
    },
    /// A commit attempted a program-counter step outside the legal transition
    /// table (architecture §5).
    #[error("illegal pc transition: {from:?} -> {to:?} is not a legal step")]
    IllegalTransition {
        /// The program counter the machine holds.
        from: Pc,
        /// The program counter the commit tried to move to.
        to: Pc,
    },
    /// Caller error: bad namespace, unknown parent entry, duplicate id, etc.
    #[error("invalid commit: {0}")]
    Invalid(String),
    /// An effect-sandwich invariant was violated (machine.rs guards).
    #[error("effect sandwich violation: {0}")]
    Effect(String),
    /// Another process holds the session's writer lock.
    #[error("session is open for writing by {}", match .holder { Some(pid) => format!("pid {pid}"), None => "another process".to_string() })]
    Locked {
        /// Pid recorded in the lock file (best effort).
        holder: Option<u32>,
    },
    /// The file changed under this handle (a writer that ignored the lock);
    /// appending would duplicate or interleave records.
    #[error("session file changed under this handle: {0}; re-open the session")]
    StaleHandle(String),
    /// Underlying I/O failure.
    #[error(transparent)]
    Io(#[from] std::io::Error),
    /// Underlying (de)serialization failure.
    #[error(transparent)]
    Json(#[from] serde_json::Error),
}

/// A usage ledger row (cost accounting per provider attempt).
///
/// Token counts are provider-reported; `cost_usd` is derived at write time.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct UsageRecord {
    /// Unique usage id (`u_<seq>` when auto-assigned).
    pub id: String,
    /// The entry this usage row is attached to.
    #[serde(rename = "entryId")]
    pub entry_id: String,
    /// Provider-reported token usage object (schema owned by the provider).
    pub usage: Value,
    /// Request cost in USD, if known at write time.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cost_usd: Option<f64>,
}

/// One atomic transaction: entries + register writes + ≤1 state transition.
///
/// This mirrors the effect-sandwich discipline: persist intent (entry +
/// pending register), execute effect, persist settlement (entry + register
/// clear + pc advance) — each phase is one commit.
#[derive(Debug, Clone, Default)]
pub struct Commit {
    /// New entries to append (validated: parent ids must exist).
    pub entries: Vec<NewEntry>,
    /// Register mutations to apply.
    pub registers: Vec<RegisterWrite>,
    /// Optional CAS state transition.
    pub transition: Option<StateTransition>,
    /// Optional usage ledger row.
    pub usage: Option<UsageRecord>,
    /// When set, the line is written and flushed but its `sync_data` is
    /// deferred to the next durable commit (#483): one `fdatasync` covers
    /// every earlier appended byte. Only for commits the caller always
    /// follows with a durable one before any effect or return.
    pub defer_sync: bool,
}

impl Commit {
    /// An empty commit (no-op).
    pub fn new() -> Self {
        Self::default()
    }

    /// Appends an entry input.
    pub fn entry(mut self, e: NewEntry) -> Self {
        self.entries.push(e);
        self
    }

    /// Appends a register write.
    pub fn register(mut self, w: RegisterWrite) -> Self {
        self.registers.push(w);
        self
    }

    /// Sets the CAS state transition.
    pub fn transition(mut self, t: StateTransition) -> Self {
        self.transition = Some(t);
        self
    }

    /// Defers this commit's `sync_data` to the next durable commit.
    pub fn defer_sync(mut self) -> Self {
        self.defer_sync = true;
        self
    }

    /// Attaches a usage ledger row.
    pub fn usage(mut self, u: UsageRecord) -> Self {
        self.usage = Some(u);
        self
    }
}

/// The single storage abstraction every caller programs against.
///
/// Backends (JSONL default, optional SQLite behind a feature flag) implement
/// this trait; callers must never contain backend-specific code.
pub trait Storage {
    /// Session id (matches the file stem).
    fn session_id(&self) -> &str;

    /// B2: the system prompt pinned at session creation, if any.
    /// Backends persist it in the header so resume re-applies it.
    fn system_prompt(&self) -> Option<&str> {
        None
    }

    /// The operator alias (issue #216), if the session was created with
    /// `run --name`. Default None so other backends stay unaffected.
    fn session_name(&self) -> Option<&str> {
        None
    }

    /// In-memory snapshot of the machine state (pc + seq).
    fn state(&self) -> MachineState;

    /// The last committed global sequence number.
    fn last_seq(&self) -> u64;

    /// All committed entries in commit order.
    fn entries(&self) -> &[Entry];

    /// Looks up an entry by id.
    fn entry(&self, id: &str) -> Option<&Entry>;

    /// Children of an entry, in commit order (branching = siblings).
    fn children(&self, parent_id: &str) -> Vec<&Entry>;

    /// Reads a register cell.
    fn get_register(&self, namespace: &str, key: &str) -> Option<&Value>;

    /// Enumerates live cells in a namespace.
    fn list_register(&self, namespace: &str) -> Vec<(&str, &Value)>;

    /// All usage rows in seq order (committed this session + replayed).
    fn usages(&self) -> &[UsageRecord];

    /// Applies one atomic commit; returns the committed entries.
    ///
    /// Rejects stale CAS tokens (`StaleTransition`) instead of silently
    /// overwriting; rejects unknown parent ids and invalid namespaces
    /// (`Invalid`) before any byte is written.
    fn commit(&mut self, c: Commit) -> Result<Vec<Entry>, StorageError>;

    /// Rewrites the file keeping only live registers (compaction).
    ///
    /// Entries are never dropped by compaction (they are the audit log);
    /// only superseded register values and deleted cells are reclaimed.
    /// Optional operation: backends may no-op.
    fn compact(&mut self) -> Result<(), StorageError>;
}

// ---------------------------------------------------------------------------
// JSONL wire records
// ---------------------------------------------------------------------------

/// Header line: first physical line of every session file.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct HeaderLine {
    v: u32,
    kind: String,
    id: String,
    #[serde(rename = "storageVersion")]
    storage_version: u32,
    created_at: u64,
    #[serde(default)]
    cwd: String,
    /// B2: system prompt pinned at session creation. Optional + skipped
    /// when absent, so pre-B2 files replay unchanged.
    #[serde(
        default,
        rename = "systemPrompt",
        skip_serializing_if = "Option::is_none"
    )]
    system_prompt: Option<String>,
    /// Operator alias (issue #216): `run --name batch-1`. Optional +
    /// skipped when absent, so pre-name files replay unchanged.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    name: Option<String>,
    /// Issue #210: where a forked session was cut from. Optional +
    /// skipped when absent, so pre-fork files replay unchanged.
    #[serde(
        default,
        rename = "forkedFrom",
        skip_serializing_if = "Option::is_none"
    )]
    forked_from: Option<ForkOrigin>,
}

/// Where a forked session was cut from (issue #210).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ForkOrigin {
    /// The source session id.
    pub session: String,
    /// The settled entry the fork was cut at (the last one it carries).
    pub entry: String,
}

/// Discriminated record inside a commit line (array element or solo object).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind")]
enum Record {
    #[serde(rename = "entry")]
    Entry {
        seq: u64,
        id: String,
        #[serde(rename = "parentId", default, skip_serializing_if = "Option::is_none")]
        parent_id: Option<String>,
        #[serde(rename = "type")]
        entry_type: String,
        timestamp: u64,
        payload: Value,
        /// Names of the secrets this entry's payload hid (#596); absent in
        /// older logs and when nothing was redacted.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        redacted: Vec<String>,
    },
    #[serde(rename = "register")]
    Register {
        op: RegisterOp,
        seq: u64,
        namespace: String,
        key: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        value: Option<Value>,
    },
    #[serde(rename = "usage")]
    Usage {
        seq: u64,
        id: String,
        #[serde(rename = "entryId")]
        entry_id: String,
        usage: Value,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cost_usd: Option<f64>,
    },
    #[serde(rename = "state")]
    State {
        seq: u64,
        pc: Pc,
        /// `true` only for the materialized state record written by
        /// `compact()` — it summarizes "the machine is at `pc`" and is
        /// exempt from transition-legality replay checks. Live commits
        /// always write `snapshot: false`.
        #[serde(default)]
        snapshot: bool,
    },
}

impl Record {
    fn seq(&self) -> u64 {
        match self {
            Record::Entry { seq, .. }
            | Record::Register { seq, .. }
            | Record::Usage { seq, .. }
            | Record::State { seq, .. } => *seq,
        }
    }
}

// ---------------------------------------------------------------------------
// JSONL backend
// ---------------------------------------------------------------------------

/// The single session-id rule (issue #295): non-empty, at most 64 bytes,
/// `[A-Za-z0-9_-]` only. The id is joined into a file path, so this also
/// guarantees no `/`, `\`, `..` or other traversal. Shared by the
/// storage boundary and every host (CLI, serve, ACP, MCP).
pub fn is_valid_session_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 64
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_'))
}

/// JSONL session file backend (default).
///
/// [`JsonlStorage::create`] starts a fresh session file,
/// [`JsonlStorage::open`] replays an existing one. One writer per file at a
/// time is the caller's responsibility (the host owns the session dir).
pub struct JsonlStorage {
    session_id: String,
    path: PathBuf,
    /// The session directory, held without following a symlink (#639), and
    /// the file's name in it: every later open, rename and sync goes through
    /// the handle, so swapping a directory on `path` for a link cannot
    /// redirect the session file, its sidecar or the compaction temp file.
    dir: SafeDir,
    file_name: OsString,
    writer: BufWriter<File>,
    /// Exclusive advisory lock on the sidecar `<id>.jsonl.lock` (never
    /// renamed, so it survives `compact()`); `None` for read-only handles.
    /// Released by the OS on drop or crash.
    _lock: Option<File>,
    /// File length after the last append this handle made or replayed;
    /// a mismatch before an append means someone else wrote.
    good_len: u64,
    created_at: u64,
    cwd: String,
    system_prompt: Option<String>,
    /// Operator alias (issue #216), pinned in the header.
    name: Option<String>,
    /// Issue #210: the fork origin pinned in the header.
    forked_from: Option<ForkOrigin>,
    /// Set when an append previously failed mid-write (cora full-scan
    /// #53): the file may now end in a newline-less fragment, so ANY
    /// further commit could glue its line onto the fragment and brick
    /// the session. Poisoned storage rejects commits with a fatal error
    /// until the host re-opens/re-creates the session.
    poisoned: Option<StorageError>,
    /// `sync_data` calls issued by appends (measurement hook, #483).
    syncs: u64,
    // In-memory state (the file is a replay recipe, not the state):
    entries: Vec<Entry>,
    by_id: BTreeMap<String, usize>,
    children: BTreeMap<String, Vec<usize>>,
    registers: BTreeMap<(String, String), Value>,
    usage: Vec<UsageRecord>,
    state: MachineState,
    /// Per entry id, the names its payload hid (kept for `compact()`).
    redacted: BTreeMap<String, Vec<String>>,
    /// The redaction set; `None`: the process set, read per commit.
    secrets: Option<SecretSet>,
}

/// The redacted form of an entry payload and the names of the secrets it
/// hid (#596). Every string is redacted structurally, keys included (and a
/// secret that is a bare number becomes the marker string), in every form
/// the set knows: as is, JSON-escaped, base64 and percent-encoded.
fn redact_payload(set: &SecretSet, payload: &Value) -> (Value, Vec<String>) {
    if set.is_empty() {
        return (payload.clone(), Vec::new());
    }
    let out = set.redact_value(payload);
    if out == *payload {
        return (out, Vec::new());
    }
    let (before, after) = (payload.to_string(), out.to_string());
    let names = set
        .names()
        .into_iter()
        .filter(|n| {
            let m = format!("«{n}»");
            after.matches(&m).count() > before.matches(&m).count()
        })
        .map(str::to_string)
        .collect();
    (out, names)
}

impl JsonlStorage {
    /// Redacts with `set` instead of the process set (tests, embedders).
    pub fn set_secrets(&mut self, set: SecretSet) {
        self.secrets = Some(set);
    }

    fn redaction_set(&self) -> SecretSet {
        self.secrets
            .clone()
            .unwrap_or_else(crate::secrets::process_secrets)
    }

    /// Creates a new session file (fails if the path already exists).
    pub fn create(
        dir: impl AsRef<Path>,
        session_id: impl Into<String>,
        cwd: Option<String>,
    ) -> Result<Self, StorageError> {
        Self::create_with(dir, session_id, cwd, None)
    }

    /// B2: create with a pinned system prompt (persisted in the header,
    /// applied to every provider request for this session).
    pub fn create_with(
        dir: impl AsRef<Path>,
        session_id: impl Into<String>,
        cwd: Option<String>,
        system_prompt: Option<&str>,
    ) -> Result<Self, StorageError> {
        Self::create_named(dir, session_id, cwd, system_prompt, None)
    }

    /// Issue #216: create with a pinned system prompt AND an operator
    /// alias (`run --name`). Additive header field — pre-name files
    /// replay unchanged.
    pub fn create_named(
        dir: impl AsRef<Path>,
        session_id: impl Into<String>,
        cwd: Option<String>,
        system_prompt: Option<&str>,
        name: Option<&str>,
    ) -> Result<Self, StorageError> {
        Self::create_inner(dir, session_id, cwd, system_prompt, name, None)
    }

    fn create_inner(
        dir: impl AsRef<Path>,
        session_id: impl Into<String>,
        cwd: Option<String>,
        system_prompt: Option<&str>,
        name: Option<&str>,
        forked_from: Option<ForkOrigin>,
    ) -> Result<Self, StorageError> {
        let session_id = session_id.into();
        // Issue #248 (rescan #145): the id is joined into a file path —
        // the storage layer enforces the safe charset itself instead of
        // trusting every caller to validate first (hosts already do via
        // validate_session_id; this is the boundary backstop).
        let id_ok = is_valid_session_id(&session_id);
        if !id_ok {
            return Err(StorageError::Io(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!("invalid session id: {session_id:?}"),
            )));
        }
        let file_name = OsString::from(format!("{session_id}.jsonl"));
        let path = dir.as_ref().join(&file_name);
        // Resolved once; every file below is opened relative to it (#639).
        let dir_h = SafeDir::open(dir.as_ref())?;
        let created_at = now_ms();
        let cwd = cwd.unwrap_or_default();
        // The pinned prompt is re-sent on resume: store what the provider
        // is shown (#596).
        let system_prompt =
            system_prompt.map(|p| crate::secrets::process_secrets().redact(p).into_owned());
        let system_prompt = system_prompt.as_deref();
        let header = HeaderLine {
            v: 1,
            kind: "header".into(),
            id: session_id.clone(),
            storage_version: STORAGE_VERSION,
            created_at,
            cwd: cwd.clone(),
            system_prompt: system_prompt.map(str::to_string),
            name: name.map(str::to_string),
            forked_from: forked_from.clone(),
        };
        let mut writer = BufWriter::new(open_in(
            &dir_h,
            &file_name,
            Mode {
                write: true,
                create_new: true,
                ..Mode::default()
            },
        )?);
        let lock = lock_writer(&dir_h, &file_name)?;
        let header_line = serde_json::to_string(&header)?;
        writeln!(writer, "{header_line}")?;
        writer.flush()?;
        Ok(Self {
            session_id,
            path,
            dir: dir_h,
            file_name,
            writer,
            _lock: Some(lock),
            good_len: header_line.len() as u64 + 1,
            created_at,
            cwd,
            name: name.map(str::to_string),
            forked_from,
            system_prompt: system_prompt.map(str::to_string),
            entries: Vec::new(),
            by_id: BTreeMap::new(),
            children: BTreeMap::new(),
            registers: BTreeMap::new(),
            usage: Vec::new(),
            state: MachineState::default(),
            redacted: BTreeMap::new(),
            secrets: None,
            poisoned: None,
            syncs: 0,
        })
    }

    /// Issue #210: starts session `new_id` in `dir` as a fork of the
    /// session file `src`: a copy of its lineage up to and including the
    /// settled entry `at` (default: the last settled entry). Settled = a
    /// final assistant message (no tool call hangs off it), the only point
    /// where the machine rests at `Final` with no open tool sandwich. The source is only read, never
    /// touched; entry ids and parents are kept, so every reader (replay,
    /// todo hydration) works on the fork unchanged. Usage rows and
    /// registers are NOT copied: a fork starts with a clean ledger.
    pub fn fork(
        src: impl AsRef<Path>,
        dir: impl AsRef<Path>,
        new_id: impl Into<String>,
        at: Option<&str>,
    ) -> Result<Self, StorageError> {
        let new_id = new_id.into();
        let from = Self::open_read_only(src)?;
        let settled = |e: &Entry| {
            e.kind.as_str() == EntryType::MESSAGE
                && e.payload["role"] == "assistant"
                && from.children(&e.id).is_empty()
        };
        let cut = match at {
            Some(id) => {
                let e = from
                    .entry(id)
                    .ok_or_else(|| StorageError::Invalid(format!("unknown entry: {id}")))?;
                if !settled(e) {
                    return Err(StorageError::Invalid(format!(
                        "entry {id} is not a settled point (a final assistant message with no open tool calls)"
                    )));
                }
                from.by_id[id]
            }
            None => from
                .entries
                .iter()
                .rposition(settled)
                .ok_or_else(|| StorageError::Invalid("no settled entry to fork from".into()))?,
        };
        let origin = ForkOrigin {
            session: from.session_id.clone(),
            entry: from.entries[cut].id.clone(),
        };
        let mut out = Self::create_inner(
            dir,
            new_id,
            Some(from.cwd.clone()),
            from.system_prompt.as_deref(),
            None,
            Some(origin),
        )?;
        let copied: Result<(), StorageError> = (|| {
            let mut c = Commit::new();
            for e in &from.entries[..=cut] {
                c = c.entry(NewEntry {
                    id: Some(e.id.clone()),
                    parent_id: e.parent_id.clone(),
                    kind: e.kind.clone(),
                    payload: e.payload.clone(),
                    timestamp: e.timestamp,
                });
            }
            out.commit(c)?;
            // Idle -> Planning -> Final: the fork rests where a finished
            // turn rests, so `resume <id> "<prompt>"` re-opens it.
            for pc in [Pc::Planning, Pc::Final] {
                let seq = out.state.seq;
                out.commit(Commit::new().transition(StateTransition::from(seq, pc)))?;
            }
            Ok(())
        })();
        if let Err(e) = copied {
            // Never leave a half-written fork behind.
            let _ = std::fs::remove_file(&out.path);
            return Err(e);
        }
        Ok(out)
    }

    /// Working directory recorded in the session header (empty if none).
    pub fn cwd(&self) -> &str {
        &self.cwd
    }

    /// Issue #210: where this session was forked from, if it is a fork.
    pub fn forked_from(&self) -> Option<&ForkOrigin> {
        self.forked_from.as_ref()
    }

    /// Opens and replays an existing session file.
    ///
    /// Older format versions replay cleanly; a newer version is a hard
    /// error. A torn final line (crash mid-append) is discarded whole.
    ///
    /// The file is opened once: the replay, the torn-tail repair and every
    /// later append use that one handle, never the path again. On unix the
    /// open does not follow a symlink at the last path component and does
    /// not block on a FIFO, and the handle must be a regular file, so a
    /// file swapped between a caller's check and this open is refused here
    /// (#581).
    pub fn open(path: impl AsRef<Path>) -> Result<Self, StorageError> {
        Self::open_with(path.as_ref(), true, None)
    }

    /// [`JsonlStorage::open`] for a session file in a directory another
    /// process can write (a cron job's workdir, #581): also refuses a file
    /// longer than `max_bytes`, checked on the opened handle, and stops
    /// reading at the cap if the file grows during the replay.
    pub fn open_bounded(path: impl AsRef<Path>, max_bytes: u64) -> Result<Self, StorageError> {
        Self::open_with(path.as_ref(), true, Some(max_bytes))
    }

    /// Replays a session file for READING only (listings, status): a torn
    /// final line is skipped in memory but never truncated on disk, so a
    /// reader can never cut a line another process is still appending
    /// (scan 2026-10-28). Do not commit through the result.
    pub fn open_read_only(path: impl AsRef<Path>) -> Result<Self, StorageError> {
        Self::open_with(path.as_ref(), false, None)
    }

    fn open_with(path: &Path, repair: bool, cap: Option<u64>) -> Result<Self, StorageError> {
        let path = path.to_path_buf();
        // The directory is resolved once and held without following a link
        // (#639); the session file and its sidecar open relative to it.
        let (dir_h, file_name) = SafeDir::for_file(&path)?;
        // Lock BEFORE reading or repairing: the torn-tail truncation below
        // would cut a line another process is still appending.
        let lock = if repair {
            Some(lock_writer(&dir_h, &file_name)?)
        } else {
            None
        };
        // One handle for the whole life of this storage. Read-only replays
        // get a read-only handle: a commit through them fails instead of
        // writing.
        let file = open_in(
            &dir_h,
            &file_name,
            Mode {
                read: true,
                append: repair,
                ..Mode::default()
            },
        )?;
        let too_large =
            |cap: u64| StorageError::Invalid(format!("session file larger than {cap} bytes"));
        if let Some(cap) = cap {
            if file.metadata()?.len() > cap {
                return Err(too_large(cap));
            }
        }
        // The reader is a duplicate of the handle (same open file); appends
        // go to the end whatever the read offset. Reading stops one byte past
        // the cap, so a file that grows during the replay is caught below.
        let limit = cap.map_or(u64::MAX, |c| c.saturating_add(1));
        let mut reader = BufReader::new(file.try_clone()?.take(limit));

        // Header (line 1, mandatory). Read with read_until: a header line
        // WITHOUT its trailing newline (crash mid-header-write) is a torn
        // write exactly like a torn final line — parse it for the error
        // message, but refuse to open for append: good_bytes = 0 makes the
        // truncation below remove the whole file, so the next append never
        // glues itself onto a newline-less header and bricks the session
        // (cora full-scan #23).
        let mut header_buf: Vec<u8> = Vec::new();
        reader.read_until(b'\n', &mut header_buf)?;
        let line = String::from_utf8(header_buf.clone())
            .map_err(|_| StorageError::Corrupt("header: invalid UTF-8".into()))?;
        if line.trim().is_empty() {
            return Err(StorageError::Corrupt("empty file (missing header)".into()));
        }
        let header: HeaderLine = serde_json::from_str(line.trim_end())
            .map_err(|e| StorageError::Corrupt(format!("bad header: {e}")))?;
        if header.kind != "header" {
            return Err(StorageError::Corrupt(format!(
                "first line is not a header (kind={})",
                header.kind
            )));
        }
        if header.storage_version > STORAGE_VERSION {
            return Err(StorageError::UnsupportedVersion {
                file: header.storage_version,
                supported: STORAGE_VERSION,
            });
        }
        // Torn header: complete JSON but no newline. This file can never
        // be appended to safely (a body line would glue onto the
        // newline-less header), and an append-style writer over an empty
        // file would produce a headerless file that bricks the NEXT
        // open(). Fail explicitly: the host's recovery path is
        // create_with(), which rewrites the file with a fresh complete
        // header (cora full-scan #23, second-round fix).
        if !header_buf.ends_with(b"\n") {
            return Err(StorageError::Corrupt(
                "torn header (missing newline); recreate the session with create_with()".into(),
            ));
        }
        let mut good_bytes = header_buf.len() as u64;

        let mut out = Self {
            session_id: header.id.clone(),
            path: path.clone(),
            dir: dir_h,
            file_name,
            writer: BufWriter::new(file),
            _lock: lock,
            good_len: 0,
            created_at: header.created_at,
            cwd: header.cwd.clone(),
            name: header.name.clone(),
            forked_from: header.forked_from.clone(),
            system_prompt: header.system_prompt.clone(),
            entries: Vec::new(),
            by_id: BTreeMap::new(),
            children: BTreeMap::new(),
            registers: BTreeMap::new(),
            usage: Vec::new(),
            state: MachineState::default(),
            redacted: BTreeMap::new(),
            secrets: None,
            poisoned: None,
            syncs: 0,
        };

        // Body lines: solo object or array (one transaction per line).
        // Manual read loop so a torn FINAL line (crash mid-append) can be
        // detected and discarded whole; corruption in a non-final line is a
        // hard error. `good_bytes` tracks the end of the last complete line
        // so a torn tail can be physically truncated before appending.
        let mut lineno = 1usize;
        let mut last_seq = 0u64;
        let mut buf: Vec<u8> = Vec::new();
        loop {
            buf.clear();
            let n = reader.read_until(b'\n', &mut buf)?;
            if n == 0 {
                break; // clean EOF
            }
            lineno += 1;
            // No trailing newline = the write was torn (every committed
            // line is written with \n). Discard whole, leave bytes out of
            // good_bytes so they get truncated below.
            if !buf.ends_with(b"\n") {
                break; // torn tail
            }
            // Count the complete physical line NOW — before any `continue`
            // below — so good_bytes never under-counts (an empty or skipped
            // line is still bytes that must never be truncated away).
            let line = match std::str::from_utf8(&buf) {
                Ok(s) => s.trim_end().to_string(),
                Err(_) => {
                    return Err(StorageError::Corrupt(format!(
                        "line {lineno}: invalid UTF-8"
                    )));
                }
            };
            if line.is_empty() {
                good_bytes += buf.len() as u64;
                continue;
            }
            let parsed: Result<Vec<Record>, serde_json::Error> = if line.starts_with('[') {
                serde_json::from_str(&line)
            } else {
                serde_json::from_str::<Record>(&line).map(|r| vec![r])
            };
            let records = match parsed {
                Ok(r) => {
                    // Count the complete physical line only after it has
                    // parsed (scan #47): counting before the parse made a
                    // newline-terminated invalid final line look fully
                    // consumed, so the truncation below never fired and the
                    // next append bricked the session forever.
                    good_bytes += buf.len() as u64;
                    r
                }
                Err(e) => {
                    if is_at_eof(&mut reader) {
                        break; // torn/unparseable final line: discard whole
                    }
                    return Err(StorageError::Corrupt(format!("line {lineno}: {e}")));
                }
            };
            for rec in records {
                let seq = rec.seq();
                if seq <= last_seq {
                    return Err(StorageError::Corrupt(format!(
                        "line {lineno}: seq {seq} not strictly increasing (last {last_seq})"
                    )));
                }
                last_seq = seq;
                out.apply(&rec, lineno)?;
            }
        }
        // A torn tail was discarded during replay. Physically truncate the
        // partial bytes NOW, before the append handle concatenates a new
        // commit line onto the fragment (would corrupt the file forever).
        if let Some(cap) = cap {
            if reader.get_ref().limit() == 0 {
                return Err(too_large(cap));
            }
        }
        // Through the handle, not the path: the file truncated is the one
        // replayed.
        let handle = out.writer.get_ref();
        if repair && good_bytes < handle.metadata()?.len() {
            handle.set_len(good_bytes)?;
            handle.sync_data()?;
            out.dir.sync()?;
        }
        // The global seq advances with every record, transition or not.
        out.state.seq = out.state.seq.max(last_seq);
        out.good_len = good_bytes;
        Ok(out)
    }

    /// Applies a replayed record to the in-memory maps (no I/O).
    fn apply(&mut self, rec: &Record, lineno: usize) -> Result<(), StorageError> {
        match rec {
            Record::Entry {
                seq,
                id,
                parent_id,
                entry_type,
                timestamp,
                payload,
                redacted,
            } => {
                if !redacted.is_empty() {
                    self.redacted.insert(id.clone(), redacted.clone());
                }
                let idx = self.entries.len();
                self.entries.push(Entry {
                    seq: *seq,
                    id: id.clone(),
                    parent_id: parent_id.clone(),
                    kind: EntryType::from_string(entry_type.clone()),
                    timestamp: *timestamp,
                    payload: payload.clone(),
                });
                if let Some(parent) = parent_id {
                    self.children.entry(parent.clone()).or_default().push(idx);
                }
                if self.by_id.insert(id.clone(), idx).is_some() {
                    return Err(StorageError::Corrupt(format!(
                        "line {lineno}: duplicate entry id {id}"
                    )));
                }
                Ok(())
            }
            Record::Register {
                op,
                seq: _,
                namespace,
                key,
                value,
            } => {
                let cell = (namespace.clone(), key.clone());
                match op {
                    RegisterOp::Set => {
                        self.registers
                            .insert(cell, value.clone().unwrap_or(Value::Null));
                    }
                    RegisterOp::Delete => {
                        self.registers.remove(&cell);
                    }
                }
                Ok(())
            }
            Record::Usage {
                seq: _,
                id,
                entry_id,
                usage,
                cost_usd,
            } => {
                self.usage.push(UsageRecord {
                    id: id.clone(),
                    entry_id: entry_id.clone(),
                    usage: usage.clone(),
                    cost_usd: *cost_usd,
                });
                Ok(())
            }
            Record::State { seq, pc, snapshot } => {
                // Replay enforces the same legality table as live commits.
                // A file whose recorded steps walk outside the table is
                // corrupt (hand-edited or written by a broken build).
                // Compaction snapshots are exempt: they summarize the
                // final position, they are not steps.
                if !*snapshot && !crate::state::can_transition(self.state.pc, *pc) {
                    return Err(StorageError::IllegalTransition {
                        from: self.state.pc,
                        to: *pc,
                    });
                }
                self.state = MachineState { pc: *pc, seq: *seq };
                Ok(())
            }
        }
    }

    /// Number of `sync_data` calls this handle's appends have issued
    /// (measurement hook for the #483 fsync-budget test).
    #[doc(hidden)]
    pub fn sync_count(&self) -> u64 {
        self.syncs
    }

    /// Durably appends one physical line.
    ///
    /// The line is flushed and fsynced so a crash never loses a *committed*
    /// record. (A crash mid-line leaves a torn tail, which open() discards
    /// whole — see the torn-tail test.) With `durable == false` the fsync is
    /// skipped; the next durable append's `sync_data` persists it too.
    fn append_line(&mut self, line: &str, durable: bool) -> Result<(), StorageError> {
        // One fstat: the file must still end where our last append did.
        let actual = self.writer.get_ref().metadata()?.len();
        if actual != self.good_len {
            return Err(StorageError::StaleHandle(format!(
                "expected {} bytes, found {actual}",
                self.good_len
            )));
        }
        let res = (|| -> Result<(), StorageError> {
            writeln!(self.writer, "{line}")?;
            self.writer.flush()?;
            if durable {
                self.writer.get_ref().sync_data()?;
                self.syncs += 1;
            }
            Ok(())
        })();
        if let Err(e) = res {
            // The write may have landed PARTIALLY (classic: ENOSPC): the
            // file can now end in a newline-less fragment. Poison the
            // storage — commit()'s gate refuses every later append so a
            // retry can never glue its line onto the fragment (cora
            // full-scan #53). In-memory state stays untouched and the
            // fragment is discarded by the next open()'s torn-tail rule.
            self.poisoned = Some(StorageError::Io(std::io::Error::other(e.to_string())));
            return Err(e);
        }
        self.good_len += line.len() as u64 + 1;
        Ok(())
    }

    /// Serializes a commit into its wire line and returns it with the final seq.
    ///
    /// `committed` entries already carry their assigned seqs; register /
    /// state / usage records continue the sequence from the last entry.
    fn encode_commit(
        &self,
        c: &Commit,
        committed: &[Entry],
        names: &[Vec<String>],
        seq_after_entries: u64,
    ) -> Result<(String, u64), StorageError> {
        let mut records: Vec<Record> = Vec::new();
        let mut next = seq_after_entries;
        for (e, redacted) in committed.iter().zip(names) {
            records.push(Record::Entry {
                seq: e.seq,
                id: e.id.clone(),
                parent_id: e.parent_id.clone(),
                entry_type: e.kind.as_str().to_string(),
                timestamp: e.timestamp,
                payload: e.payload.clone(),
                redacted: redacted.clone(),
            });
        }
        for w in &c.registers {
            next += 1;
            records.push(Record::Register {
                op: w.op,
                seq: next,
                namespace: w.namespace.clone(),
                key: w.key.clone(),
                value: w.value.clone(),
            });
        }
        if let Some(t) = &c.transition {
            next += 1;
            records.push(Record::State {
                seq: next,
                pc: t.new_pc,
                snapshot: false,
            });
        }
        if let Some(u) = &c.usage {
            next += 1;
            records.push(Record::Usage {
                seq: next,
                id: if u.id.is_empty() {
                    format!("u_{next}")
                } else {
                    u.id.clone()
                },
                entry_id: u.entry_id.clone(),
                usage: u.usage.clone(),
                cost_usd: u.cost_usd,
            });
        }
        let line = if records.len() == 1 {
            serde_json::to_string(&records[0])?
        } else {
            serde_json::to_string(&records)?
        };
        Ok((line, next))
    }
}

/// Returns `true` when the reader is at EOF (used to classify a parse
/// failure on the final line as a torn write rather than corruption).
fn is_at_eof(reader: &mut impl Read) -> bool {
    let mut probe = [0u8; 1];
    matches!(reader.read(&mut probe), Ok(0))
}

/// Takes the exclusive writer lock on the sidecar `<path>.lock` and records
/// our pid in it (best effort). A sidecar, not the session file: `compact()`
/// renames a new file over the live path, which would drop a lock held on
/// the old inode.
fn lock_writer(dir: &SafeDir, file_name: &OsStr) -> Result<File, StorageError> {
    let mut name = file_name.to_owned();
    name.push(".lock");
    // CLOEXEC (set by `open_plain`): a spawned child must not inherit the
    // writer lock. Without it, an flock held on the fd survives into every
    // child until it exits (run_command, job_start, nohup-style wrappers),
    // and a later `resume` is refused with a pid that no longer holds the
    // session. NOFOLLOW and the regular-file check: the pid write below
    // truncates the file, so a planted symlink must not lead it elsewhere
    // (#581).
    let mut f = open_in(
        dir,
        &name,
        Mode {
            read: true,
            write: true,
            create: true,
            ..Mode::default()
        },
    )?;
    let mut waited = std::time::Duration::ZERO;
    loop {
        match f.try_lock() {
            Ok(()) => {
                // The pid is advisory text; a failed write never blocks the open.
                // Rewind first: an earlier attempt read the file, and
                // `set_len` does not move the cursor, so without this the pid
                // would land past the old end and the file be NUL-padded.
                let _ = f
                    .set_len(0)
                    .and_then(|()| f.seek(SeekFrom::Start(0)).map(|_| ()))
                    .and_then(|()| write!(f, "{}", std::process::id()));
                return Ok(f);
            }
            Err(std::fs::TryLockError::WouldBlock) => {
                let mut text = String::new();
                let _ = f.seek(SeekFrom::Start(0));
                let _ = f.read_to_string(&mut text);
                let holder: Option<u32> = text.trim().parse().ok();
                // #552: an flock belongs to the open file description, so a
                // child forked by ANOTHER thread of this process briefly
                // holds a duplicate of a descriptor we just closed (until it
                // execs). A hold that names OUR pid is either that transient
                // or a genuine second open in this process (#475): retry for
                // a bounded moment, which costs the genuine case almost
                // nothing. Any other holder fails at once, as before.
                if holder == Some(std::process::id()) && waited < SELF_HOLD_RETRY {
                    std::thread::sleep(SELF_HOLD_STEP);
                    waited += SELF_HOLD_STEP;
                    continue;
                }
                return Err(StorageError::Locked { holder });
            }
            Err(std::fs::TryLockError::Error(e)) => return Err(e.into()),
        }
    }
}

/// How long [`lock_writer`] keeps retrying a lock that names our own pid
/// (see #552), in steps of [`SELF_HOLD_STEP`].
const SELF_HOLD_RETRY: std::time::Duration = std::time::Duration::from_millis(150);
const SELF_HOLD_STEP: std::time::Duration = std::time::Duration::from_millis(5);

/// Opens `name` in `dir` for a session handle (#581, #639). The open is
/// relative to the directory handle, so no directory on the way can be a
/// swapped link; it does not follow a symlink at `name` (`O_NOFOLLOW`) and
/// does not wait for a FIFO's other end (`O_NONBLOCK`, which has no effect
/// on a regular file's reads and writes). The handle must be a regular file:
/// the check is an `fstat` of the opened handle, so the file checked is the
/// file used.
fn open_in(dir: &SafeDir, name: &OsStr, mode: Mode) -> Result<File, StorageError> {
    dir.open_file(name, mode).map_err(|e| match e.kind() {
        std::io::ErrorKind::InvalidInput => StorageError::Invalid(e.to_string()),
        _ => StorageError::Io(e),
    })
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

impl Storage for JsonlStorage {
    fn session_id(&self) -> &str {
        &self.session_id
    }

    fn system_prompt(&self) -> Option<&str> {
        self.system_prompt.as_deref()
    }

    /// A `fact/session_name` register (TUI `/name`, #461) wins over the
    /// header alias: the header is append-only history, the register is the
    /// rename.
    fn session_name(&self) -> Option<&str> {
        self.get_register("fact", "session_name")
            .and_then(Value::as_str)
            .filter(|n| !n.is_empty())
            .or(self.name.as_deref())
    }

    fn state(&self) -> MachineState {
        self.state
    }

    fn last_seq(&self) -> u64 {
        self.state.seq
    }

    fn entries(&self) -> &[Entry] {
        &self.entries
    }

    fn entry(&self, id: &str) -> Option<&Entry> {
        self.by_id.get(id).map(|&i| &self.entries[i])
    }

    fn children(&self, parent_id: &str) -> Vec<&Entry> {
        self.children
            .get(parent_id)
            .map(|idxs| idxs.iter().map(|&i| &self.entries[i]).collect())
            .unwrap_or_default()
    }

    fn get_register(&self, namespace: &str, key: &str) -> Option<&Value> {
        self.registers
            .get(&(namespace.to_string(), key.to_string()))
    }

    fn list_register(&self, namespace: &str) -> Vec<(&str, &Value)> {
        self.registers
            .iter()
            .filter(|((ns, _), _)| ns.as_str() == namespace)
            .map(|((_, k), v)| (k.as_str(), v))
            .collect()
    }

    fn usages(&self) -> &[UsageRecord] {
        &self.usage
    }

    fn commit(&mut self, c: Commit) -> Result<Vec<Entry>, StorageError> {
        // Poison gate (cora full-scan #53): a previous append failed
        // mid-write, so the file may end in a newline-less fragment. Any
        // further append could glue onto it and brick the session —
        // refuse until the host re-opens (which replays only complete
        // lines) or re-creates the session.
        if let Some(p) = &self.poisoned {
            return Err(StorageError::Corrupt(format!(
                "storage poisoned by an earlier failed append ({p}); re-open or re-create the session"
            )));
        }
        // Validate before writing any byte.
        for w in &c.registers {
            if !is_valid_namespace(&w.namespace) {
                return Err(StorageError::Invalid(format!(
                    "unknown namespace family: {}",
                    w.namespace
                )));
            }
            if w.op == RegisterOp::Set && w.value.is_none() {
                return Err(StorageError::Invalid(format!(
                    "register set without value: {}/{}",
                    w.namespace, w.key
                )));
            }
        }
        if let Some(t) = &c.transition {
            if t.expected_seq != self.state.seq {
                return Err(StorageError::StaleTransition {
                    expected: t.expected_seq,
                    actual: self.state.seq,
                });
            }
            if !crate::state::can_transition(self.state.pc, t.new_pc) {
                return Err(StorageError::IllegalTransition {
                    from: self.state.pc,
                    to: t.new_pc,
                });
            }
        }

        // Materialize entries (assign seq + auto ids) while validating.
        // Intra-commit parents work: each entry's id (explicit or auto
        // `e_<seq>`) becomes visible to the entries that follow it in the
        // same commit line. Deterministic and side-effect free, so a
        // rejection here happens before any byte is written.
        //
        // O(new entries), not O(total entries) (cora full-scan #52):
        // parent checks use by_id.contains_key() directly, and only the
        // CURRENT commit's ids go into a small local set — cloning the
        // whole by_id index per commit made sessions O(N²) overall.
        let mut new_ids: std::collections::HashSet<String> = std::collections::HashSet::new();
        let mut committed: Vec<Entry> = Vec::with_capacity(c.entries.len());
        // #596: redacted BEFORE encoding; memory keeps the same form.
        let set = self.redaction_set();
        let mut names: Vec<Vec<String>> = Vec::with_capacity(c.entries.len());
        let mut next = self.state.seq;
        for e in &c.entries {
            if let Some(p) = &e.parent_id {
                let known = self.by_id.contains_key(p) || new_ids.contains(p);
                if !known {
                    return Err(StorageError::Invalid(format!("unknown parent entry: {p}")));
                }
            }
            next += 1;
            let id = e.id.clone().unwrap_or_else(|| format!("e_{next}"));
            if self.by_id.contains_key(&id) || !new_ids.insert(id.clone()) {
                return Err(StorageError::Invalid(format!("duplicate entry id: {id}")));
            }
            let (payload, hid) = redact_payload(&set, &e.payload);
            names.push(hid);
            committed.push(Entry {
                seq: next,
                id,
                parent_id: e.parent_id.clone(),
                kind: e.kind.clone(),
                payload,
                timestamp: if e.timestamp == 0 {
                    now_ms()
                } else {
                    e.timestamp
                },
            });
        }

        // Encode + append (single physical line = atomic transaction).
        let (line, final_seq) = self.encode_commit(&c, &committed, &names, next)?;
        self.append_line(&line, !c.defer_sync)?;

        // Apply to memory only after the byte hit the file.
        for (e, hid) in committed.iter().zip(names) {
            if !hid.is_empty() {
                self.redacted.insert(e.id.clone(), hid);
            }
            let idx = self.entries.len();
            self.entries.push(e.clone());
            if let Some(p) = &e.parent_id {
                self.children.entry(p.clone()).or_default().push(idx);
            }
            self.by_id.insert(e.id.clone(), idx);
        }
        if let Some(u) = &c.usage {
            let id = if u.id.is_empty() {
                format!("u_{final_seq}")
            } else {
                u.id.clone()
            };
            self.usage.push(UsageRecord {
                id,
                entry_id: u.entry_id.clone(),
                usage: u.usage.clone(),
                cost_usd: u.cost_usd,
            });
        }
        for w in &c.registers {
            let cell = (w.namespace.clone(), w.key.clone());
            match w.op {
                RegisterOp::Set => self
                    .registers
                    .insert(cell, w.value.clone().unwrap_or(Value::Null)),
                RegisterOp::Delete => self.registers.remove(&cell),
            };
        }
        if let Some(t) = c.transition {
            self.state = MachineState {
                pc: t.new_pc,
                seq: final_seq,
            };
        } else {
            self.state.seq = final_seq;
        }
        Ok(committed)
    }

    fn compact(&mut self) -> Result<(), StorageError> {
        // Rewrite: header + live entries (original seqs preserved — the audit
        // log is never renumbered) + live registers + usage + final state.
        let mut records: Vec<Record> = Vec::new();
        let mut seq = self.state.seq;
        for e in &self.entries {
            records.push(Record::Entry {
                seq: e.seq,
                id: e.id.clone(),
                parent_id: e.parent_id.clone(),
                entry_type: e.kind.as_str().to_string(),
                timestamp: e.timestamp,
                payload: e.payload.clone(),
                redacted: self.redacted.get(&e.id).cloned().unwrap_or_default(),
            });
        }
        for ((ns, k), v) in &self.registers {
            seq += 1;
            records.push(Record::Register {
                op: RegisterOp::Set,
                seq,
                namespace: ns.clone(),
                key: k.clone(),
                value: Some(v.clone()),
            });
        }
        for u in &self.usage {
            seq += 1;
            records.push(Record::Usage {
                seq,
                id: u.id.clone(),
                entry_id: u.entry_id.clone(),
                usage: u.usage.clone(),
                cost_usd: u.cost_usd,
            });
        }
        seq += 1;
        records.push(Record::State {
            seq,
            pc: self.state.pc,
            snapshot: true,
        });

        // Write to temp file, fsync, then atomic rename over the live path,
        // then fsync the directory so the rename itself survives a crash.
        let tmp = PathBuf::from(&self.file_name).with_extension("jsonl.tmp");
        {
            // Never through a symlink planted at the temp path (#581), and
            // in the held directory whatever happened to the path (#639).
            let mut w = BufWriter::new(open_in(
                &self.dir,
                tmp.as_os_str(),
                Mode {
                    write: true,
                    create: true,
                    truncate: true,
                    ..Mode::default()
                },
            )?);
            let header = HeaderLine {
                v: 1,
                kind: "header".into(),
                id: self.session_id.clone(),
                storage_version: STORAGE_VERSION,
                created_at: self.created_at,
                cwd: self.cwd.clone(),
                name: self.name.clone(),
                forked_from: self.forked_from.clone(),
                system_prompt: self.system_prompt.clone(),
            };
            writeln!(w, "{}", serde_json::to_string(&header)?)?;
            for r in &records {
                writeln!(w, "{}", serde_json::to_string(r)?)?;
            }
            w.flush()?;
            w.get_ref().sync_data()?;
        }
        self.dir.rename(tmp.as_os_str(), &self.file_name)?;
        self.dir.sync()?;
        // Reopen the writer on the new file. If THIS reopen fails, the
        // rename has already happened: the old writer now points at an
        // unlinked inode, so any further commit would vanish silently.
        // Mark the session poisoned — every subsequent commit fails
        // LOUDLY instead of losing data (CodeCora scan-3: silent loss).
        self.writer = BufWriter::new(
            match open_in(
                &self.dir,
                &self.file_name,
                Mode {
                    append: true,
                    ..Mode::default()
                },
            ) {
                Ok(w) => w,
                Err(e) => {
                    self.poisoned = Some(StorageError::Invalid(format!(
                        "compact(): reopen after rename failed: {e}"
                    )));
                    return Err(e);
                }
            },
        );
        self.good_len = self.writer.get_ref().metadata()?.len();
        self.state.seq = seq;
        // The file was atomically rewritten from COMPLETE records only —
        // any torn fragment that justified the poison no longer exists.
        // Clear the poison (CodeCora round-2 on the #53 fix): leaving it
        // set would permanently disable a session whose file was just
        // verifiably repaired. This makes compact() the sanctioned
        // recovery path for a poisoned session.
        self.poisoned = None;
        Ok(())
    }
}

#[cfg(all(test, unix))]
mod poison_tests {
    use super::*;
    use crate::entry::{EntryType, NewEntry};
    use serde_json::json;
    use std::fs::OpenOptions;

    /// Unique per invocation: pid alone collides across concurrent or
    /// pid-reusing runs sharing the temp dir.
    fn uniq_dir(tag: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!(
            "tole-{tag}-{}-{:x}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ))
    }

    fn msg(v: Value) -> NewEntry {
        NewEntry::root(EntryType::new(EntryType::MESSAGE), v)
    }

    /// Inject a REAL mid-write failure (ENOSPC) by re-pointing the
    /// writer at /dev/full, then drive the poison contract end-to-end:
    /// the failing commit errors, every later commit hits the poison
    /// gate, in-memory state stays untouched, and a re-open discards
    /// the fragment and works again. Direct field access is the honest
    /// injection point here — chmod-based injection is ineffective
    /// because create() already holds a write-enabled fd (cora ronde-2
    /// caught the chmod variant as a false-coverage MAJOR).
    /// Issue #248 regression: the storage boundary itself refuses ids
    /// that could escape the sessions dir — no caller can skip the
    /// validation by forgetting it upstream.
    #[test]
    fn create_named_refuses_unsafe_session_ids() {
        let dir = uniq_dir("sid");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        for bad in [
            "../../evil",
            "/abs/path",
            "a/b",
            "a\\b",
            "..",
            "",
            "a b",
            "id;rm",
        ] {
            assert!(
                JsonlStorage::create_named(&dir, bad, None, None, None).is_err(),
                "must refuse: {bad:?}"
            );
        }
        // The safe charset still works.
        assert!(JsonlStorage::create_named(&dir, "s-abc123", None, None, None).is_ok());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Issue #295: one shared rule — uppercase and `_` are valid,
    /// traversal and over-long ids are not.
    #[test]
    fn is_valid_session_id_table() {
        let max = "a".repeat(64);
        for ok in ["s-abc123", "ABC", "a_b", "Mixed_Case-1", max.as_str()] {
            assert!(is_valid_session_id(ok), "must accept: {ok:?}");
        }
        let long = "a".repeat(65);
        for bad in ["", "..", "../x", "a/b", "a\\b", "a.b", "a b", long.as_str()] {
            assert!(!is_valid_session_id(bad), "must reject: {bad:?}");
        }
    }

    #[test]
    fn failed_append_poisons_until_reopen() {
        let dir = uniq_dir("poison");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let mut s = JsonlStorage::create(&dir, "p", None).unwrap();
        s.commit(Commit::new().entry(msg(json!({ "n": 1 }))))
            .unwrap();

        // Swap the writer to /dev/full: every write() returns ENOSPC.
        // Linux-only inside the test body (the unix gate covers macOS/
        // FreeBSD where /dev/full does not exist) — skip honestly there
        // instead of panicking (CodeCora CI annotation).
        match OpenOptions::new().write(true).open("/dev/full") {
            Ok(f) => {
                s.writer = BufWriter::new(f);
                s.good_len = 0; // /dev/full is empty: keep the stale-handle check quiet
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                eprintln!("/dev/full unavailable on this platform — poison path untested here");
                let _ = std::fs::remove_dir_all(&dir);
                return;
            }
            Err(e) => panic!("unexpected error opening /dev/full: {e}"),
        }

        let first = s.commit(Commit::new().entry(msg(json!({ "n": 2 }))));
        assert!(first.is_err(), "append to /dev/full must fail");

        // Poisoned: the retry (the exact bug #53 describes) is refused.
        let second = s
            .commit(Commit::new().entry(msg(json!({ "n": 3 }))))
            .unwrap_err();
        assert!(
            second.to_string().contains("poisoned"),
            "second commit must hit the poison gate, got: {second}"
        );
        // In-memory state was never mutated by the failed commits.
        assert_eq!(s.entries().len(), 1);
        assert_eq!(s.last_seq(), 1);

        // compact() is the sanctioned recovery (CodeCora round-2): it
        // atomically rewrites the file from complete records only, so
        // the poison clears and commits work again WITHOUT a reopen.
        // The snapshot carries seq = 2 (header + snapshot line); the
        // post-compact commit continues from there as seq 3.
        s.compact().unwrap();
        s.commit(Commit::new().entry(msg(json!({ "n": 2 }))))
            .unwrap();
        assert_eq!(s.entries().len(), 2);
        assert_eq!(s.last_seq(), 3);

        // Recovery: reopen the REAL file — replay now includes the
        // compact snapshot AND the post-compact commit; the session is
        // fully usable again.
        drop(s);
        let path = dir.join("p.jsonl");
        let mut reopened = JsonlStorage::open(&path).unwrap();
        assert_eq!(reopened.entries().len(), 2);
        reopened
            .commit(Commit::new().entry(msg(json!({ "n": 3 }))))
            .unwrap();
        assert_eq!(reopened.last_seq(), 4);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// O(new) commit validation (#52): intra-commit parents resolve via
    /// the local new-id set, duplicates across commits still rejected.
    #[test]
    fn intra_commit_parents_and_duplicate_ids_still_validated() {
        let dir = uniq_dir("p_ids");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let mut s = JsonlStorage::create(&dir, "ids", None).unwrap();
        // parent references an earlier entry IN THE SAME commit.
        let c = Commit::new()
            .entry(msg(json!({ "role": "parent" })))
            .entry(NewEntry::with_parent(
                "e_1",
                EntryType::new(EntryType::MESSAGE),
                json!({ "role": "child" }),
            ));
        s.commit(c).unwrap();
        assert_eq!(s.entries().len(), 2);
        // Duplicate id across commits is still refused.
        let dup = NewEntry {
            id: Some("e_1".into()),
            parent_id: None,
            kind: EntryType::new(EntryType::MESSAGE),
            payload: json!({}),
            timestamp: 0,
        };
        assert!(s
            .commit(Commit::new().entry(dup))
            .unwrap_err()
            .to_string()
            .contains("duplicate"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
