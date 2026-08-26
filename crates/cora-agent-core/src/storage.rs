//! Storage trait + JSONL session backend (default).
//!
//! One file per session (`<session-id>.jsonl`). The file is a **replay
//! recipe**, not the state: opening replays lines into in-memory maps and all
//! queries run in RAM. Writes append one physical line per commit (an array
//! line groups one atomic transaction); a torn final line is discarded whole.
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
use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, BufWriter, Read, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::entry::{Entry, EntryType, NewEntry};
use crate::register::{is_valid_namespace, RegisterOp, RegisterWrite};
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

/// JSONL session file backend (default).
///
/// [`JsonlStorage::create`] starts a fresh session file,
/// [`JsonlStorage::open`] replays an existing one. One writer per file at a
/// time is the caller's responsibility (the host owns the session dir).
pub struct JsonlStorage {
    session_id: String,
    path: PathBuf,
    writer: BufWriter<File>,
    created_at: u64,
    cwd: String,
    // In-memory state (the file is a replay recipe, not the state):
    entries: Vec<Entry>,
    by_id: BTreeMap<String, usize>,
    children: BTreeMap<String, Vec<usize>>,
    registers: BTreeMap<(String, String), Value>,
    usage: Vec<UsageRecord>,
    state: MachineState,
}

impl JsonlStorage {
    /// Creates a new session file (fails if the path already exists).
    pub fn create(
        dir: impl AsRef<Path>,
        session_id: impl Into<String>,
        cwd: Option<String>,
    ) -> Result<Self, StorageError> {
        let session_id = session_id.into();
        let path = dir.as_ref().join(format!("{session_id}.jsonl"));
        let created_at = now_ms();
        let cwd = cwd.unwrap_or_default();
        let header = HeaderLine {
            v: 1,
            kind: "header".into(),
            id: session_id.clone(),
            storage_version: STORAGE_VERSION,
            created_at,
            cwd: cwd.clone(),
        };
        let mut writer = BufWriter::new(
            OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&path)?,
        );
        writeln!(writer, "{}", serde_json::to_string(&header)?)?;
        writer.flush()?;
        Ok(Self {
            session_id,
            path,
            writer,
            created_at,
            cwd,
            entries: Vec::new(),
            by_id: BTreeMap::new(),
            children: BTreeMap::new(),
            registers: BTreeMap::new(),
            usage: Vec::new(),
            state: MachineState::default(),
        })
    }

    /// Opens and replays an existing session file.
    ///
    /// Older format versions replay cleanly; a newer version is a hard
    /// error. A torn final line (crash mid-append) is discarded whole.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, StorageError> {
        let path = path.as_ref().to_path_buf();
        let file = File::open(&path)?;
        let mut reader = BufReader::new(file);

        // Header (line 1, mandatory).
        let mut line = String::new();
        reader.read_line(&mut line)?;
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

        let mut out = Self {
            session_id: header.id.clone(),
            path: path.clone(),
            writer: BufWriter::new(OpenOptions::new().append(true).open(&path)?),
            created_at: header.created_at,
            cwd: header.cwd.clone(),
            entries: Vec::new(),
            by_id: BTreeMap::new(),
            children: BTreeMap::new(),
            registers: BTreeMap::new(),
            usage: Vec::new(),
            state: MachineState::default(),
        };

        // Body lines: solo object or array (one transaction per line).
        // Manual read loop so a torn FINAL line (crash mid-append) can be
        // detected and discarded whole; corruption in a non-final line is a
        // hard error. `good_bytes` tracks the end of the last complete line
        // so a torn tail can be physically truncated before appending.
        let mut lineno = 1usize;
        let mut last_seq = 0u64;
        let mut good_bytes = line.len() as u64;
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
            good_bytes += buf.len() as u64;
            let line = match std::str::from_utf8(&buf) {
                Ok(s) => s.trim_end().to_string(),
                Err(_) => {
                    return Err(StorageError::Corrupt(format!(
                        "line {lineno}: invalid UTF-8"
                    )));
                }
            };
            if line.is_empty() {
                continue;
            }
            let parsed: Result<Vec<Record>, serde_json::Error> = if line.starts_with('[') {
                serde_json::from_str(&line)
            } else {
                serde_json::from_str::<Record>(&line).map(|r| vec![r])
            };
            let records = match parsed {
                Ok(r) => r,
                Err(e) => {
                    if is_at_eof(&mut reader) {
                        break; // torn final line: discard whole
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
        if good_bytes < file_len(&out.path)? {
            truncate_file(&out.path, good_bytes)?;
            sync_dir(&out.path)?;
        }
        // The global seq advances with every record, transition or not.
        out.state.seq = out.state.seq.max(last_seq);
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
            } => {
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

    /// Durably appends one physical line.
    ///
    /// The line is flushed and fsynced so a crash never loses a *committed*
    /// record. (A crash mid-line leaves a torn tail, which open() discards
    /// whole — see the torn-tail test.)
    fn append_line(&mut self, line: &str) -> Result<(), StorageError> {
        writeln!(self.writer, "{line}")?;
        self.writer.flush()?;
        self.writer.get_ref().sync_data()?;
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
        seq_after_entries: u64,
    ) -> Result<(String, u64), StorageError> {
        let mut records: Vec<Record> = Vec::new();
        let mut next = seq_after_entries;
        for e in committed {
            records.push(Record::Entry {
                seq: e.seq,
                id: e.id.clone(),
                parent_id: e.parent_id.clone(),
                entry_type: e.kind.as_str().to_string(),
                timestamp: e.timestamp,
                payload: e.payload.clone(),
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
fn is_at_eof(reader: &mut BufReader<File>) -> bool {
    let mut probe = [0u8; 1];
    matches!(reader.read(&mut probe), Ok(0))
}

/// File length in bytes (used to detect a discarded torn tail on open).
fn file_len(path: &Path) -> std::io::Result<u64> {
    Ok(std::fs::metadata(path)?.len())
}

/// Truncates the file to `len` bytes and fsyncs it.
fn truncate_file(path: &Path, len: u64) -> std::io::Result<()> {
    let f = OpenOptions::new().write(true).truncate(false).open(path)?;
    f.set_len(len)?;
    f.sync_data()?;
    Ok(())
}

/// Durability: flush a rename into the parent directory so the directory
/// entry itself survives a crash. POSIX-only; on non-Unix targets opening
/// the directory is not possible and we accept the platform's guarantees.
#[cfg(unix)]
fn sync_dir(path: &Path) -> std::io::Result<()> {
    use std::os::unix::ffi::OsStrExt;
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().as_bytes().is_empty())
        .unwrap_or_else(|| Path::new("."));
    File::open(parent)?.sync_all()
}

#[cfg(not(unix))]
fn sync_dir(_path: &Path) -> std::io::Result<()> {
    Ok(())
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
        let mut known_ids: std::collections::HashSet<String> = self.by_id.keys().cloned().collect();
        let mut committed: Vec<Entry> = Vec::with_capacity(c.entries.len());
        let mut next = self.state.seq;
        for e in &c.entries {
            if let Some(p) = &e.parent_id {
                if !known_ids.contains(p) {
                    return Err(StorageError::Invalid(format!("unknown parent entry: {p}")));
                }
            }
            next += 1;
            let id = e.id.clone().unwrap_or_else(|| format!("e_{next}"));
            if !known_ids.insert(id.clone()) {
                return Err(StorageError::Invalid(format!("duplicate entry id: {id}")));
            }
            committed.push(Entry {
                seq: next,
                id,
                parent_id: e.parent_id.clone(),
                kind: e.kind.clone(),
                payload: e.payload.clone(),
                timestamp: if e.timestamp == 0 {
                    now_ms()
                } else {
                    e.timestamp
                },
            });
        }

        // Encode + append (single physical line = atomic transaction).
        let (line, final_seq) = self.encode_commit(&c, &committed, next)?;
        self.append_line(&line)?;

        // Apply to memory only after the byte hit the file.
        for e in committed.iter() {
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
        let tmp = self.path.with_extension("jsonl.tmp");
        {
            let mut w = BufWriter::new(File::create(&tmp)?);
            let header = HeaderLine {
                v: 1,
                kind: "header".into(),
                id: self.session_id.clone(),
                storage_version: STORAGE_VERSION,
                created_at: self.created_at,
                cwd: self.cwd.clone(),
            };
            writeln!(w, "{}", serde_json::to_string(&header)?)?;
            for r in &records {
                writeln!(w, "{}", serde_json::to_string(r)?)?;
            }
            w.flush()?;
            w.get_ref().sync_data()?;
        }
        std::fs::rename(&tmp, &self.path)?;
        sync_dir(&self.path)?;
        // Reopen the writer on the new file.
        self.writer = BufWriter::new(OpenOptions::new().append(true).open(&self.path)?);
        self.state.seq = seq;
        Ok(())
    }
}
