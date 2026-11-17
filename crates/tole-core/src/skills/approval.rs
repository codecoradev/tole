//! Owner-approved skill content (#591, design `skills-registry-design.md`
//! §4.2): a skill is followed, so the trust anchor is the owner's approval
//! of its exact bytes, not the fact that a file exists or that an index
//! returned its name.
//!
//! - A skill **package** is every file of its directory (SKILL.md plus
//!   `references/`, `scripts/`, `assets/`, ...). Its hash covers each
//!   file's relative path and bytes, so any change to any file needs a new
//!   approval. A symlink in a package makes it unreadable (its target could
//!   change without the hash changing).
//! - **Approvals** live in the user's data dir
//!   (`$CODECORA_HOME/tole/skill-approvals.json`), outside every workspace,
//!   one record per skill name: the approved hash, when, and how many files.
//!   `tole skills approve <name>` writes it; nothing else does.
//! - A [`Gate`] decides what a session may load. [`Gate::Open`] is today's
//!   behavior. [`Gate::Approved`] loads a body only when the package hash on
//!   disk matches the approval, and parses it from the same bytes it hashed;
//!   any other skill is reported as unavailable, with no description and no
//!   body.

use crate::skills::{
    parse_loaded, read_capped, read_file_capped, skill_file, ReadCapError, Skill,
    MAX_SKILL_FILE_BYTES,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// The approvals file name in the tole data dir.
pub const APPROVALS_FILE: &str = "skill-approvals.json";
/// Most files a skill package may hold.
pub const MAX_PACKAGE_FILES: usize = 256;
/// Most bytes a skill package may hold, all files together.
pub const MAX_PACKAGE_BYTES: u64 = 4 * 1024 * 1024;
/// Most directory entries (files and directories, at any depth) a skill
/// package walk visits.
pub const MAX_PACKAGE_ENTRIES: usize = 1024;
/// Largest approvals file read.
pub const MAX_APPROVALS_BYTES: u64 = 1024 * 1024;

/// The size limits of a package walk ([`MAX_PACKAGE_FILES`],
/// [`MAX_PACKAGE_BYTES`] outside tests).
#[derive(Debug, Clone, Copy)]
struct Limits {
    files: usize,
    bytes: u64,
    /// Directory entries (files and directories) walked.
    entries: usize,
}

const LIMITS: Limits = Limits {
    files: MAX_PACKAGE_FILES,
    bytes: MAX_PACKAGE_BYTES,
    entries: MAX_PACKAGE_ENTRIES,
};

/// The append-only audit ledger next to the approvals file (#600): one line
/// per approve and revoke, never rewritten. It is authoritative over the
/// approvals file for every skill it names (see [`check_against_ledger`]).
pub const LEDGER_FILE: &str = "skill-ledger.jsonl";
/// Largest ledger read.
pub const MAX_LEDGER_BYTES: u64 = 16 * 1024 * 1024;
/// Longest source URL or registry id kept.
const MAX_SOURCE_FIELD: usize = 512;

/// Where a third-party skill came from, recorded as data (#600). Nothing
/// here is fetched or verified: it is what the owner states at approval, so
/// a later review can see the claimed origin next to the approved bytes.
/// The trust anchor stays the hash of the bytes.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Source {
    /// Where the skill was pulled from (a URL).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    /// Its id in the registry that listed it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub registry_id: Option<String>,
    /// The commit it was pinned at: 40 or 64 lowercase hex digits.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sha: Option<String>,
}

impl Source {
    /// True when no field is set.
    pub fn is_empty(&self) -> bool {
        self.url.is_none() && self.registry_id.is_none() && self.sha.is_none()
    }

    /// Refuses control characters, over-long fields and a malformed SHA.
    pub fn validate(&self) -> Result<(), String> {
        for (what, v) in [
            ("source url", &self.url),
            ("registry id", &self.registry_id),
        ] {
            if let Some(v) = v {
                if v.is_empty()
                    || v.chars().count() > MAX_SOURCE_FIELD
                    || v.chars().any(char::is_control)
                {
                    return Err(format!(
                        "{what}: 1 to {MAX_SOURCE_FIELD} characters, no control characters"
                    ));
                }
            }
        }
        if let Some(sha) = &self.sha {
            let hex = sha
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b));
            if !hex || !matches!(sha.len(), 40 | 64) {
                return Err("pinned sha: 40 or 64 lowercase hex digits".into());
            }
        }
        Ok(())
    }
}

/// The owner's approval of one skill.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Approval {
    /// The approved package hash (`sha256:<hex>`).
    pub hash: String,
    /// Seconds since the Unix epoch.
    pub approved_at: u64,
    /// Files in the approved package.
    pub files: usize,
    /// The stated origin of a third-party skill (data, never verified).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<Source>,
}

/// Every approval of this user, by skill name.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Approvals {
    #[serde(default)]
    pub skills: BTreeMap<String, Approval>,
}

impl Approvals {
    /// `$CODECORA_HOME/tole/skill-approvals.json`; `None` when the CodeCora
    /// home is unresolvable (then nothing is approved).
    pub fn default_path() -> Option<PathBuf> {
        crate::paths::tole_data_dir().map(|d| d.join(APPROVALS_FILE))
    }

    /// Read `path`. A missing file is no approvals; an unreadable or
    /// malformed one is an error (fail closed, loudly). The ledger
    /// checkpoint (see [`LedgerCheckpoint`]) is not checked here: that is
    /// [`Approvals::load_with_checkpoint`] plus [`LedgerCheckpoint::verify`].
    pub fn load(path: &Path) -> Result<Self, String> {
        Self::load_with_checkpoint(path).map(|(a, _)| a)
    }

    /// [`Approvals::load`] and the ledger checkpoint recorded in the same
    /// file (`None`: a file written before #879, or by [`Approvals::save`]
    /// of an embedder that never wrote one). Both come from the same bytes.
    pub fn load_with_checkpoint(path: &Path) -> Result<(Self, Option<LedgerCheckpoint>), String> {
        match read_file_capped(path, MAX_APPROVALS_BYTES) {
            Ok(raw) => {
                let bad = |e: serde_json::Error| {
                    format!("{}: malformed skill approvals: {e}", path.display())
                };
                let approvals = serde_json::from_slice(&raw).map_err(bad)?;
                let side: CheckpointField = serde_json::from_slice(&raw).map_err(bad)?;
                Ok((approvals, side.ledger_checkpoint))
            }
            Err(ReadCapError::Io(e)) if e.kind() == std::io::ErrorKind::NotFound => {
                Ok((Self::default(), None))
            }
            Err(e) => Err(format!("{}: {e}", path.display())),
        }
    }

    /// Write `path` atomically (temp file + rename), creating its directory.
    ///
    /// **This does not touch the skill ledger.** An embedder that calls
    /// [`Approvals::approve`] (or edits `skills`) and `save`s skips the
    /// append-only audit record: the approval loads, but the ledger shows it
    /// as unrecorded (and a revoke done this way leaves no revoke line). An
    /// existing ledger checkpoint is kept as it is, never dropped, so this
    /// cannot downgrade a checkpointed install. To record a decision, append
    /// a [`ledger_line`] to the ledger first and then call
    /// [`Approvals::save_checkpointed`], as `tole skills approve` does.
    pub fn save(&self, path: &Path) -> Result<(), String> {
        let keep = Self::load_with_checkpoint(path).ok().and_then(|(_, c)| c);
        self.write(path, keep.as_ref())
    }

    /// The write that records the ledger: verifies the checkpoint already in
    /// `path` against the ledger as it is now (an intact ledger only grew,
    /// which is fine), then writes the approvals with a new checkpoint of
    /// that same ledger read. Call it AFTER appending the ledger line, so a
    /// crash in between leaves the ledger longer than the checkpoint (valid)
    /// and never the reverse. Refuses, writing nothing, when the existing
    /// checkpoint no longer matches the ledger (see [`LedgerCheckpoint`]).
    pub fn save_checkpointed(&self, path: &Path) -> Result<LedgerCheckpoint, String> {
        let old = Self::load_with_checkpoint(path).map(|(_, c)| c)?;
        let ledger = read_ledger_bytes(path)?;
        if let Some(old) = &old {
            old.verify(&ledger)
                .map_err(|why| fault_message(path, &why))?;
        }
        let cp = LedgerCheckpoint::of(&ledger);
        self.write(path, Some(&cp))?;
        Ok(cp)
    }

    fn write(&self, path: &Path, checkpoint: Option<&LedgerCheckpoint>) -> Result<(), String> {
        let dir = path.parent().unwrap_or_else(|| Path::new("."));
        std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
        let mut value = serde_json::to_value(self).map_err(|e| e.to_string())?;
        if let (Some(cp), Some(obj)) = (checkpoint, value.as_object_mut()) {
            obj.insert(
                CHECKPOINT_KEY.into(),
                serde_json::to_value(cp).map_err(|e| e.to_string())?,
            );
        }
        let json = serde_json::to_vec_pretty(&value).map_err(|e| e.to_string())?;
        let tmp = dir.join(format!(".{APPROVALS_FILE}.{}.tmp", std::process::id()));
        std::fs::write(&tmp, json).map_err(|e| format!("{}: {e}", tmp.display()))?;
        std::fs::rename(&tmp, path).map_err(|e| {
            let _ = std::fs::remove_file(&tmp);
            format!("{}: {e}", path.display())
        })
    }

    /// Owner recovery after a [`LedgerCheckpoint`] fault: records `expect`
    /// (the digest the owner reviewed) as the checkpoint of the ledger as it
    /// is now, WITHOUT verifying the old one. Refuses when the ledger is no
    /// longer exactly what `expect` describes. The approvals themselves are
    /// not changed; no revoke the old ledger held is brought back.
    pub fn accept_ledger(path: &Path, expect: &LedgerCheckpoint) -> Result<(), String> {
        let (approvals, _) = Self::load_with_checkpoint(path)?;
        let ledger = read_ledger_bytes(path)?;
        if LedgerCheckpoint::of(&ledger) != *expect {
            return Err(
                "the skill ledger changed while it was being reviewed: review it again".into(),
            );
        }
        approvals.write(path, Some(expect))
    }

    /// Approve `pkg` as it is on disk now, replacing an earlier approval of
    /// the same name (only the latest approved bytes load).
    pub fn approve(&mut self, pkg: &SkillPackage, now: u64) {
        self.approve_from(pkg, now, None);
    }

    /// [`Approvals::approve`] recording where the skill came from.
    pub fn approve_from(&mut self, pkg: &SkillPackage, now: u64, source: Option<Source>) {
        self.skills.insert(
            pkg.name.clone(),
            Approval {
                hash: pkg.hash.clone(),
                approved_at: now,
                files: pkg.files.len(),
                source: source.filter(|s| !s.is_empty()),
            },
        );
    }

    /// Drop the approval of `name`; false when there was none.
    pub fn revoke(&mut self, name: &str) -> bool {
        self.skills.remove(name).is_some()
    }

    /// True when `hash` is the approved hash of `name`.
    pub fn is_approved(&self, name: &str, hash: &str) -> bool {
        self.skills.get(name).is_some_and(|a| a.hash == hash)
    }
}

/// One audit-ledger line (`approve` or `revoke`): the skill name, the
/// package hash and, for an approval, the stated source. Names, hashes and
/// the owner's source fields only; never skill text.
pub fn ledger_line(
    event: &str,
    name: &str,
    hash: &str,
    source: Option<&Source>,
    at: u64,
) -> String {
    build_line(event, name, hash, source, at, None)
}

/// An `approve` line for an approval that already existed when the ledger
/// did not name it (`tole skills adopt`, #879): [`ledger_line`] plus
/// `"note":"adopted"`, with the hash and source exactly as the approvals
/// file stored them.
pub fn adopted_ledger_line(name: &str, hash: &str, source: Option<&Source>, at: u64) -> String {
    build_line("approve", name, hash, source, at, Some("adopted"))
}

fn build_line(
    event: &str,
    name: &str,
    hash: &str,
    source: Option<&Source>,
    at: u64,
    note: Option<&str>,
) -> String {
    let mut v = serde_json::json!({
        "v": 1, "kind": "skill", "event": event, "skill": name, "hash": hash, "at": at,
    });
    if let Some(src) = source.filter(|s| !s.is_empty()) {
        v["source"] = serde_json::to_value(src).unwrap_or_default();
    }
    if let Some(note) = note {
        v["note"] = note.into();
    }
    let mut line = v.to_string();
    line.push('\n');
    line
}

/// The ledger next to the approvals file `approvals`. `Ok("")`: none yet.
/// `Err`: it exists but cannot be read safely (fail closed: the approvals
/// cannot be checked against it).
pub fn read_ledger(approvals: &Path) -> Result<String, String> {
    read_ledger_bytes(approvals).map(|raw| String::from_utf8_lossy(&raw).into_owned())
}

/// The ledger file next to the approvals file `approvals`.
pub fn ledger_path(approvals: &Path) -> PathBuf {
    approvals
        .parent()
        .map_or_else(|| PathBuf::from(LEDGER_FILE), |d| d.join(LEDGER_FILE))
}

/// [`read_ledger`] as the raw bytes (what a [`LedgerCheckpoint`] hashes: a
/// non-UTF-8 byte changes the digest). One read, never more than
/// [`MAX_LEDGER_BYTES`]; a missing ledger is empty.
pub fn read_ledger_bytes(approvals: &Path) -> Result<Vec<u8>, String> {
    let path = ledger_path(approvals);
    match read_file_capped(&path, MAX_LEDGER_BYTES) {
        Ok(raw) => Ok(raw),
        Err(ReadCapError::Io(e)) if e.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(e) => Err(format!(
            "{}: {e}. The skill ledger must be a regular readable file under {} MiB: inspect or \
             restore it. Deleting it forgets every revoke",
            path.display(),
            MAX_LEDGER_BYTES / (1024 * 1024)
        )),
    }
}

/// The approvals file key holding the [`LedgerCheckpoint`].
pub const CHECKPOINT_KEY: &str = "ledger_checkpoint";

#[derive(Deserialize)]
struct CheckpointField {
    #[serde(default)]
    ledger_checkpoint: Option<LedgerCheckpoint>,
}

/// What the skill ledger looked like when the approvals file was last
/// written by the owner's commands (#879): its byte length and the SHA-256
/// of those bytes. The approvals file stores it under [`CHECKPOINT_KEY`].
///
/// At load, a ledger that is missing, shorter than `bytes`, or whose first
/// `bytes` bytes hash differently was deleted, truncated or rewritten by
/// something other than the append-only owner commands: every approval is
/// then ignored (fail closed) until the owner inspects the ledger and runs
/// `tole skills recheckpoint`. A ledger that only grew (the recorded prefix
/// intact) is valid, so a crash between the ledger append and the approvals
/// write is harmless.
///
/// Residual, stated plainly: this catches deleting, truncating or rewriting
/// the ledger ALONE. A process able to write BOTH the ledger and the
/// approvals file can forge a matching checkpoint (the same residual as the
/// plugin ledger), and one that can append to the ledger can add lines the
/// owner never wrote (an `approve` line still needs a matching approvals
/// entry to load anything). An approvals file without a checkpoint is the
/// pre-#879 format and keeps loading, since it is indistinguishable from an
/// old install; a downgrade by stripping the field takes a write to the
/// approvals file and, to hide a revoke, a second write to the ledger.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LedgerCheckpoint {
    /// Ledger length in bytes.
    pub bytes: u64,
    /// `sha256:<hex>` of the ledger's first `bytes` bytes.
    pub sha256: String,
}

impl LedgerCheckpoint {
    /// The checkpoint of exactly these ledger bytes.
    pub fn of(ledger: &[u8]) -> Self {
        Self {
            bytes: ledger.len() as u64,
            sha256: sha256_tag(ledger),
        }
    }

    /// Ok when `ledger` still starts with the recorded bytes (it may have
    /// grown since); otherwise what is wrong with it. `ledger` is the one
    /// buffer the caller also parses, so what is verified is what is used.
    pub fn verify(&self, ledger: &[u8]) -> Result<(), String> {
        let len = ledger.len() as u64;
        if len < self.bytes {
            return Err(if len == 0 {
                format!(
                    "it is missing or empty, but {} bytes were recorded (deleted or truncated)",
                    self.bytes
                )
            } else {
                format!(
                    "it holds {len} bytes, fewer than the {} recorded (truncated or replaced)",
                    self.bytes
                )
            });
        }
        // `bytes <= len`, and `len` is at most MAX_LEDGER_BYTES for a buffer
        // from `read_ledger_bytes`: the slice is within the buffer.
        if sha256_tag(&ledger[..self.bytes as usize]) != self.sha256 {
            return Err(format!(
                "its first {} bytes are not the ones recorded (rewritten)",
                self.bytes
            ));
        }
        Ok(())
    }
}

fn sha256_tag(bytes: &[u8]) -> String {
    let hex: String = Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    format!("sha256:{hex}")
}

/// The error for a ledger that fails its checkpoint: what is wrong and how
/// the owner recovers.
pub fn fault_message(approvals: &Path, why: &str) -> String {
    format!(
        "skill ledger {} does not match the checkpoint in {}: {why}. Every skill approval is \
         ignored until you inspect the ledger and accept it with `tole skills recheckpoint` \
         (deleting or rewriting the ledger forgets revokes: review `tole skills list` after)",
        ledger_path(approvals).display(),
        approvals.display()
    )
}

/// Checks the checkpoint in `approvals` (if it has one) against the ledger
/// next to it: `Ok(())` when there is none or it holds, the
/// [`fault_message`] otherwise. An unreadable ledger or approvals file is an
/// error too.
pub fn verify_checkpoint_at(approvals: &Path) -> Result<(), String> {
    let (_, cp) = Approvals::load_with_checkpoint(approvals)?;
    match cp {
        Some(cp) => cp
            .verify(&read_ledger_bytes(approvals)?)
            .map_err(|why| fault_message(approvals, &why)),
        None => Ok(()),
    }
}

/// What the ledger says about the approvals file.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LedgerCheck {
    /// `(skill, why)` for approvals the ledger does not allow.
    pub blocked: Vec<(String, String)>,
    /// Approved skills the ledger has no line for (approved before the
    /// ledger, or written by hand): allowed, but not vouched for.
    pub unrecorded: Vec<String>,
}

/// The last ledger decision per skill: `Some(hash)` approved, `None` revoked.
fn ledger_state(ledger: &str) -> BTreeMap<String, Option<String>> {
    let mut state = BTreeMap::new();
    for line in ledger.lines() {
        let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        if v["kind"] != "skill" {
            continue;
        }
        let Some(name) = v["skill"].as_str() else {
            continue;
        };
        match v["event"].as_str() {
            Some("approve") => {
                // An approve line without a hash vouches for nothing.
                state.insert(name.to_string(), v["hash"].as_str().map(str::to_string));
            }
            Some("revoke") => {
                state.insert(name.to_string(), None);
            }
            _ => {}
        }
    }
    state
}

/// Compares `approvals` with the ledger (the append-only record of the
/// owner's decisions): an approval for a skill whose last ledger line is a
/// revoke, or whose approved hash is not the ledger's, was written by hand
/// and is blocked. Skills the ledger never names are `unrecorded`.
pub fn check_against_ledger(approvals: &Approvals, ledger: &str) -> LedgerCheck {
    let state = ledger_state(ledger);
    let mut out = LedgerCheck::default();
    for (name, a) in &approvals.skills {
        match state.get(name) {
            None => out.unrecorded.push(name.clone()),
            Some(Some(h)) if *h == a.hash => {}
            Some(Some(h)) => out.blocked.push((
                name.clone(),
                format!("its approved hash is not the ledger's ({h})"),
            )),
            Some(None) => out.blocked.push((
                name.clone(),
                "the ledger records a revoke after its last approval".into(),
            )),
        }
    }
    out
}

/// The last `n` ledger lines (`at`, `event`, `hash`) of skill `name`, oldest
/// first.
pub fn ledger_history(ledger: &str, name: &str, n: usize) -> Vec<(u64, String, String)> {
    let mut all: Vec<(u64, String, String)> = ledger
        .lines()
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        .filter(|v| v["kind"] == "skill" && v["skill"] == name)
        .filter_map(|v| {
            Some((
                v["at"].as_u64()?,
                v["event"].as_str()?.to_string(),
                v["hash"].as_str().unwrap_or_default().to_string(),
            ))
        })
        .collect();
    let from = all.len().saturating_sub(n);
    all.drain(..from);
    all
}

/// One skill's directory, read in full: what an approval covers.
#[derive(Debug, Clone, PartialEq)]
pub struct SkillPackage {
    pub name: String,
    pub dir: PathBuf,
    /// `(relative path with '/', bytes)`, sorted by path.
    pub files: Vec<(String, Vec<u8>)>,
    /// [`package_hash`] of `files`.
    pub hash: String,
}

impl SkillPackage {
    /// The bytes of SKILL.md (always present in a package).
    pub fn skill_md(&self) -> &[u8] {
        self.files
            .iter()
            .find(|(p, _)| p == "SKILL.md")
            .map(|(_, b)| b.as_slice())
            .unwrap_or_default()
    }
}

/// The package of skill `name`, from the directory `load_skill` would read
/// (project first, then user-global).
pub fn read_package(workspace: Option<&Path>, name: &str) -> Result<SkillPackage, String> {
    let dir = skill_file(workspace, name)
        .and_then(|f| f.parent().map(Path::to_path_buf))
        .ok_or_else(|| format!("skill {name:?} not found"))?;
    let files = read_dir_files(&dir, LIMITS)?;
    let hash = package_hash(&files);
    Ok(SkillPackage {
        name: name.to_string(),
        dir,
        files,
        hash,
    })
}

/// The package of the skill directory `dir` (the curator moves packages by
/// directory, outside the workspace/user-global resolution of
/// [`read_package`]).
pub fn read_dir_package(name: &str, dir: &Path) -> Result<SkillPackage, String> {
    let files = read_dir_files(dir, LIMITS)?;
    let hash = package_hash(&files);
    Ok(SkillPackage {
        name: name.to_string(),
        dir: dir.to_path_buf(),
        files,
        hash,
    })
}

/// Every file under `dir` (relative path, bytes), sorted by path, within
/// `limits`. A file is never read past what is left of the byte budget:
/// its length is checked first (lstat), and the read itself is capped.
fn read_dir_files(dir: &Path, limits: Limits) -> Result<Vec<(String, Vec<u8>)>, String> {
    let mut files = Vec::new();
    let mut total = 0u64;
    let mut seen = 0usize;
    walk(dir, "", &mut files, &mut total, &mut seen, limits)?;
    files.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(files)
}

fn walk(
    dir: &Path,
    prefix: &str,
    files: &mut Vec<(String, Vec<u8>)>,
    total: &mut u64,
    seen: &mut usize,
    limits: Limits,
) -> Result<(), String> {
    let over = || {
        format!(
            "{}: skill package over {} files or {} bytes",
            dir.display(),
            limits.files,
            limits.bytes
        )
    };
    let entries = std::fs::read_dir(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    for entry in entries {
        let entry = entry.map_err(|e| format!("{}: {e}", dir.display()))?;
        // Every entry counts, directories too: a package of countless
        // (empty) directories is refused before it costs a long walk.
        *seen += 1;
        if *seen > limits.entries {
            return Err(format!(
                "{}: skill package over {} entries",
                dir.display(),
                limits.entries
            ));
        }
        let path = entry.path();
        let file_name = entry.file_name();
        let name = file_name
            .to_str()
            .ok_or_else(|| format!("{}: non-UTF-8 file name", path.display()))?;
        let rel = if prefix.is_empty() {
            name.to_string()
        } else {
            format!("{prefix}/{name}")
        };
        let kind = entry
            .file_type()
            .map_err(|e| format!("{}: {e}", path.display()))?;
        if kind.is_symlink() {
            return Err(format!("{}: symlink in a skill package", path.display()));
        } else if kind.is_dir() {
            walk(&path, &rel, files, total, seen, limits)?;
        } else if kind.is_file() {
            // Refuse before reading: the file count, then the length the
            // directory entry reports (lstat: a symlink is not followed).
            let remaining = limits.bytes.saturating_sub(*total);
            let len = entry
                .metadata()
                .map_err(|e| format!("{}: {e}", path.display()))?
                .len();
            if files.len() >= limits.files || len > remaining {
                return Err(over());
            }
            let file = open_no_follow(&path).map_err(|e| format!("{}: {e}", path.display()))?;
            let bytes = read_capped(file, remaining)
                .map_err(|e| format!("{}: {e}", path.display()))?
                .ok_or_else(over)?;
            *total += bytes.len() as u64;
            files.push((rel, bytes));
        } else {
            return Err(format!("{}: not a regular file", path.display()));
        }
    }
    Ok(())
}

/// Open `path` for reading without following a symlink swapped in after
/// the directory walk saw a regular file.
fn open_no_follow(path: &Path) -> std::io::Result<std::fs::File> {
    let mut opts = std::fs::OpenOptions::new();
    opts.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.custom_flags(libc::O_NOFOLLOW);
    }
    let file = opts.open(path)?;
    if !file.metadata()?.is_file() {
        return Err(std::io::Error::other("not a regular file"));
    }
    Ok(file)
}

/// `sha256:<hex>` over each file's relative path, length and bytes, in
/// the given (sorted) order. The length prefix keeps two packages from
/// sharing a byte stream.
pub fn package_hash(files: &[(String, Vec<u8>)]) -> String {
    let mut h = Sha256::new();
    for (path, bytes) in files {
        h.update((path.len() as u64).to_le_bytes());
        h.update(path.as_bytes());
        h.update((bytes.len() as u64).to_le_bytes());
        h.update(bytes);
    }
    let hex: String = h.finalize().iter().map(|b| format!("{b:02x}")).collect();
    format!("sha256:{hex}")
}

/// Why a skill could not be loaded through a [`Gate`].
#[derive(Debug, Clone, PartialEq)]
pub enum LoadError {
    /// The skill exists but its bytes are not the approved ones.
    NotApproved { name: String, hash: String },
    /// Missing, unreadable or malformed.
    Other(String),
}

impl std::fmt::Display for LoadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotApproved { name, hash } => write!(
                f,
                "skill {name:?} is not approved by the owner (content {hash}), so it cannot be \
                 loaded; the owner can review it and run `tole skills approve {name}`"
            ),
            Self::Other(e) => f.write_str(e),
        }
    }
}

/// What a session may load.
#[derive(Debug, Clone, Default, PartialEq)]
pub enum Gate {
    /// Every skill on disk (the pre-#591 behavior).
    #[default]
    Open,
    /// Only skills whose package hash matches the owner's approval.
    Approved(Arc<Approvals>),
}

impl Gate {
    /// Load skill `name`. Under [`Gate::Approved`] the body is parsed from
    /// the exact bytes that were hashed, so a file swapped after the check
    /// is never what loads.
    pub fn load(&self, workspace: Option<&Path>, name: &str) -> Result<Skill, LoadError> {
        match self {
            Self::Open => crate::skills::load_skill(workspace, name).map_err(LoadError::Other),
            Self::Approved(approvals) => {
                let pkg = read_package(workspace, name).map_err(LoadError::Other)?;
                if !approvals.is_approved(name, &pkg.hash) {
                    return Err(LoadError::NotApproved {
                        name: name.to_string(),
                        hash: pkg.hash,
                    });
                }
                if pkg.skill_md().len() as u64 > MAX_SKILL_FILE_BYTES {
                    return Err(LoadError::Other(format!(
                        "skill {name:?}: SKILL.md is over {MAX_SKILL_FILE_BYTES} bytes"
                    )));
                }
                let text = std::str::from_utf8(pkg.skill_md()).map_err(|_| {
                    LoadError::Other(format!("skill {name:?}: SKILL.md is not UTF-8"))
                })?;
                parse_loaded(text, name).map_err(LoadError::Other)
            }
        }
    }
}

/// `YYYY-MM-DD HH:MM UTC` for an approval time.
pub fn format_time(secs: u64) -> String {
    let days = (secs / 86_400) as i64;
    let (y, m, d) = crate::cron::civil_from_days(days);
    let rem = secs % 86_400;
    format!(
        "{y:04}-{m:02}-{d:02} {:02}:{:02} UTC",
        rem / 3600,
        rem % 3600 / 60
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::test_dir::TestDir;
    fn tmp(tag: &str) -> TestDir {
        TestDir::new(&format!("tole-skapprove-{tag}"))
    }

    fn write(ws: &Path, rel: &str, body: &str) {
        let p = ws.join("skills").join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, body).unwrap();
    }

    fn skill(ws: &Path, name: &str, body: &str) {
        write(
            ws,
            &format!("{name}/SKILL.md"),
            &format!("---\nname: {name}\ndescription: \"{name} d\"\n---\n{body}"),
        );
    }

    #[test]
    fn package_hash_covers_every_file_path_and_byte() {
        let ws = tmp("hash");
        skill(&ws, "demo", "b");
        write(&ws, "demo/references/a.md", "ref");
        let p1 = read_package(Some(&ws), "demo").unwrap();
        assert_eq!(
            p1.files.iter().map(|(p, _)| p.as_str()).collect::<Vec<_>>(),
            ["SKILL.md", "references/a.md"]
        );
        assert!(
            p1.hash.starts_with("sha256:") && p1.hash.len() == 71,
            "{}",
            p1.hash
        );
        // A reference changes: new hash.
        write(&ws, "demo/references/a.md", "ref2");
        let p2 = read_package(Some(&ws), "demo").unwrap();
        assert_ne!(p1.hash, p2.hash);
        // A file is renamed with the same bytes: new hash.
        std::fs::remove_file(ws.join("skills/demo/references/a.md")).unwrap();
        write(&ws, "demo/references/b.md", "ref2");
        let p3 = read_package(Some(&ws), "demo").unwrap();
        assert_ne!(p2.hash, p3.hash);
        // A file is added: new hash.
        write(&ws, "demo/scripts/x.sh", "");
        assert_ne!(read_package(Some(&ws), "demo").unwrap().hash, p3.hash);
        // Same bytes: same hash.
        assert_eq!(
            read_package(Some(&ws), "demo").unwrap().hash,
            read_package(Some(&ws), "demo").unwrap().hash
        );
        let _ = std::fs::remove_dir_all(&ws);
    }

    #[cfg(unix)]
    #[test]
    fn a_symlink_in_a_package_makes_it_unreadable() {
        let ws = tmp("link");
        skill(&ws, "demo", "b");
        std::os::unix::fs::symlink("/etc/hostname", ws.join("skills/demo/ref.md")).unwrap();
        let err = read_package(Some(&ws), "demo").unwrap_err();
        assert!(err.contains("symlink"), "{err}");
        let _ = std::fs::remove_dir_all(&ws);
    }

    #[test]
    fn the_approved_gate_loads_only_the_approved_bytes() {
        let ws = tmp("gate");
        skill(&ws, "demo", "approved body");
        skill(&ws, "other", "never approved");
        let mut a = Approvals::default();
        a.approve(&read_package(Some(&ws), "demo").unwrap(), 1);
        let gate = Gate::Approved(Arc::new(a));
        assert_eq!(gate.load(Some(&ws), "demo").unwrap().body, "approved body");
        // Never approved: not loaded, and the error names the hash.
        let other_hash = read_package(Some(&ws), "other").unwrap().hash;
        assert_eq!(
            gate.load(Some(&ws), "other").unwrap_err(),
            LoadError::NotApproved {
                name: "other".into(),
                hash: other_hash
            }
        );
        // Edited after approval (body or a reference): not loaded.
        skill(&ws, "demo", "tampered body");
        assert!(matches!(
            gate.load(Some(&ws), "demo"),
            Err(LoadError::NotApproved { .. })
        ));
        skill(&ws, "demo", "approved body");
        assert!(gate.load(Some(&ws), "demo").is_ok());
        write(&ws, "demo/references/new.md", "x");
        assert!(matches!(
            gate.load(Some(&ws), "demo"),
            Err(LoadError::NotApproved { .. })
        ));
        // An approval of one name never covers another with the same bytes.
        let mut b = Approvals::default();
        b.approve(&read_package(Some(&ws), "other").unwrap(), 1);
        b.skills.insert("demo".into(), b.skills["other"].clone());
        assert!(Gate::Approved(Arc::new(b)).load(Some(&ws), "demo").is_err());
        // Missing skill: an ordinary error. Open gate: today's behavior.
        assert!(matches!(
            gate.load(Some(&ws), "absent"),
            Err(LoadError::Other(_))
        ));
        assert_eq!(
            Gate::Open.load(Some(&ws), "other").unwrap().body,
            "never approved"
        );
        let _ = std::fs::remove_dir_all(&ws);
    }

    /// A reader that panics when asked for more than `cap` bytes in total:
    /// proof that an over-budget file is never read in full.
    struct Bounded {
        cap: usize,
        served: usize,
    }

    impl std::io::Read for Bounded {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            assert!(
                self.served + buf.len().min(1) <= self.cap,
                "read past the cap ({} bytes)",
                self.cap
            );
            let n = buf.len().min(self.cap - self.served).min(4096);
            buf[..n].fill(0);
            self.served += n;
            Ok(n)
        }
    }

    /// Cora finding on #617: a huge file must be refused without reading
    /// it into memory. The read stops one byte past the remaining budget.
    #[test]
    fn an_over_budget_file_is_refused_without_a_full_read() {
        use std::io::Read as _;
        // An endless source: only the cap keeps the read bounded.
        let endless = Bounded { cap: 11, served: 0 };
        assert_eq!(read_capped(endless, 10).unwrap(), None);
        let exact = Bounded { cap: 11, served: 0 };
        assert_eq!(read_capped(exact.take(10), 10).unwrap(), Some(vec![0; 10]));
        // The walk refuses on the reported length before opening the file,
        // and on the file count, with the over-limit error.
        let ws = tmp("limits");
        skill(&ws, "demo", "b");
        let dir = ws.join("skills/demo");
        let tiny = Limits {
            files: 8,
            bytes: 64,
            entries: 64,
        };
        assert!(read_dir_files(&dir, tiny).is_ok());
        let big = std::fs::File::create(dir.join("big.bin")).unwrap();
        big.set_len(1 << 40).unwrap(); // sparse: 1 TiB reported, nothing stored
        let err = read_dir_files(&dir, tiny).unwrap_err();
        assert!(
            err.ends_with("skill package over 8 files or 64 bytes"),
            "{err}"
        );
        assert!(read_package(Some(&ws), "demo")
            .unwrap_err()
            .ends_with(&format!(
                "skill package over {MAX_PACKAGE_FILES} files or {MAX_PACKAGE_BYTES} bytes"
            )));
        std::fs::remove_file(dir.join("big.bin")).unwrap();
        write(&ws, "demo/a.md", "a");
        let one_file = Limits {
            files: 1,
            bytes: 64,
            entries: 64,
        };
        assert!(read_dir_files(&dir, one_file)
            .unwrap_err()
            .ends_with("over 1 files or 64 bytes"));
        let _ = std::fs::remove_dir_all(&ws);
    }

    /// Review note on #617: directories count against the limits, so a
    /// package of countless empty directories is refused quickly.
    #[test]
    fn empty_directories_count_against_the_entry_limit() {
        let ws = tmp("entries");
        skill(&ws, "demo", "b");
        let dir = ws.join("skills/demo");
        for i in 0..6 {
            std::fs::create_dir_all(dir.join(format!("d{i}/e"))).unwrap();
        }
        let few = Limits {
            files: 8,
            bytes: 1024,
            entries: 8,
        };
        let err = read_dir_files(&dir, few).unwrap_err();
        assert!(err.contains("over 8 entries"), "{err}");
        let enough = Limits {
            files: 8,
            bytes: 1024,
            entries: 13,
        };
        assert!(
            read_dir_files(&dir, enough).is_ok(),
            "1 file + 12 directories"
        );
        let _ = std::fs::remove_dir_all(&ws);
    }

    #[test]
    fn approvals_round_trip_replace_revoke_and_fail_closed() {
        let ws = tmp("store");
        skill(&ws, "demo", "v1");
        let path = ws.join("data").join(APPROVALS_FILE);
        assert_eq!(Approvals::load(&path).unwrap(), Approvals::default());
        let mut a = Approvals::default();
        let v1 = read_package(Some(&ws), "demo").unwrap();
        a.approve(&v1, 1_700_000_000);
        a.save(&path).unwrap();
        let back = Approvals::load(&path).unwrap();
        assert_eq!(back, a);
        assert_eq!(back.skills["demo"].files, 1);
        // Approving new bytes replaces the old approval.
        skill(&ws, "demo", "v2");
        let v2 = read_package(Some(&ws), "demo").unwrap();
        a.approve(&v2, 1_700_000_100);
        assert!(a.is_approved("demo", &v2.hash));
        assert!(!a.is_approved("demo", &v1.hash));
        assert!(a.revoke("demo"));
        assert!(!a.revoke("demo"));
        assert!(!a.is_approved("demo", &v2.hash));
        // A malformed file is an error, never "no approvals" or "all".
        std::fs::write(&path, "{not json").unwrap();
        assert!(Approvals::load(&path).unwrap_err().contains("malformed"));
        // An oversized one is refused without reading it in full.
        std::fs::File::create(&path)
            .unwrap()
            .set_len(MAX_APPROVALS_BYTES + 1)
            .unwrap();
        let err = Approvals::load(&path).unwrap_err();
        assert!(err.contains("over 1048576 bytes"), "{err}");
        assert_eq!(format_time(1_700_000_000), "2023-11-14 22:13 UTC");
        let _ = std::fs::remove_dir_all(&ws);
    }

    fn src(sha: &str) -> Source {
        Source {
            url: Some("https://example.test/s.git".into()),
            registry_id: Some("reg/demo".into()),
            sha: Some(sha.into()),
        }
    }

    #[test]
    fn a_source_is_validated_and_kept_as_data() {
        let sha = "a".repeat(40);
        assert!(src(&sha).validate().is_ok());
        assert!(src(&"b".repeat(64)).validate().is_ok());
        for bad in ["", "abc", &"A".repeat(40), &"g".repeat(40), &"a".repeat(41)] {
            assert!(src(bad).validate().is_err(), "{bad}");
        }
        let mut s = src(&sha);
        s.url = Some("https://x\u{1b}[2J".into());
        assert!(s.validate().is_err());
        s.url = Some("x".repeat(MAX_SOURCE_FIELD + 1));
        assert!(s.validate().is_err());
        // Round trip, and an old approvals file (no `source`) still loads.
        let ws = tmp("source");
        skill(&ws, "demo", "b");
        let pkg = read_package(Some(&ws), "demo").unwrap();
        let mut a = Approvals::default();
        a.approve_from(&pkg, 1, Some(src(&sha)));
        let path = ws.join("approvals.json");
        a.save(&path).unwrap();
        assert_eq!(Approvals::load(&path).unwrap(), a);
        a.approve_from(&pkg, 2, Some(Source::default()));
        assert_eq!(a.skills["demo"].source, None, "an empty source is none");
        std::fs::write(
            &path,
            r#"{"skills":{"demo":{"hash":"sha256:x","approved_at":1,"files":1}}}"#,
        )
        .unwrap();
        assert_eq!(Approvals::load(&path).unwrap().skills["demo"].source, None);
        let _ = std::fs::remove_dir_all(&ws);
    }

    #[test]
    fn the_ledger_outranks_a_hand_edited_approvals_file() {
        let ws = tmp("ledger");
        skill(&ws, "demo", "b");
        let pkg = read_package(Some(&ws), "demo").unwrap();
        let mut a = Approvals::default();
        a.approve(&pkg, 5);
        // No ledger line: allowed, reported as unrecorded.
        let c = check_against_ledger(&a, "");
        assert_eq!(c.unrecorded, ["demo"]);
        assert!(c.blocked.is_empty());
        // Approved in the ledger with the same hash: fine.
        let mut l = ledger_line("approve", "demo", &pkg.hash, None, 5);
        assert_eq!(check_against_ledger(&a, &l), LedgerCheck::default());
        // A revoke after it: an approvals file that still holds the entry
        // (hand edit) is blocked.
        l.push_str(&ledger_line("revoke", "demo", &pkg.hash, None, 6));
        let c = check_against_ledger(&a, &l);
        assert_eq!(c.blocked.len(), 1, "{c:?}");
        // Approving again lifts it; a different hash in the file does not.
        let s = src(&"a".repeat(40));
        l.push_str(&ledger_line("approve", "demo", &pkg.hash, Some(&s), 7));
        assert_eq!(check_against_ledger(&a, &l), LedgerCheck::default());
        a.skills.get_mut("demo").unwrap().hash = "sha256:evil".into();
        assert_eq!(check_against_ledger(&a, &l).blocked.len(), 1);
        // Junk and foreign lines are ignored; history is the last n, in order.
        l.push_str("not json\n{\"kind\":\"plugin\",\"skill\":\"demo\"}\n");
        let h = ledger_history(&l, "demo", 2);
        assert_eq!(
            h.iter()
                .map(|(at, e, _)| (*at, e.as_str()))
                .collect::<Vec<_>>(),
            [(6, "revoke"), (7, "approve")]
        );
        let _ = std::fs::remove_dir_all(&ws);
    }

    #[test]
    fn an_unreadable_ledger_is_an_error_not_no_history() {
        let ws = tmp("ledger-io");
        let approvals = ws.join("approvals.json");
        assert_eq!(read_ledger(&approvals).unwrap(), "");
        std::fs::create_dir(ws.join(LEDGER_FILE)).unwrap();
        let e = read_ledger(&approvals).unwrap_err();
        assert!(
            e.contains("Deleting it forgets every revoke") && e.contains("16 MiB"),
            "{e}"
        );
        let _ = std::fs::remove_dir_all(&ws);
    }

    fn demo_approvals(ws: &Path) -> (Approvals, PathBuf) {
        skill(ws, "demo", "b");
        let pkg = read_package(Some(ws), "demo").unwrap();
        let mut a = Approvals::default();
        a.approve(&pkg, 5);
        (a, ws.join("approvals.json"))
    }

    fn append(path: &Path, bytes: &[u8]) {
        use std::io::Write as _;
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .unwrap();
        f.write_all(bytes).unwrap();
    }

    /// #879: the checkpoint holds for an intact or longer ledger and names
    /// what is wrong with a missing, shorter or rewritten one (a rewrite of
    /// the same length and non-UTF-8 bytes included).
    #[test]
    fn a_checkpoint_verifies_a_grown_ledger_and_names_every_tampering() {
        let l = ledger_line("approve", "demo", "sha256:a", None, 1).into_bytes();
        let cp = LedgerCheckpoint::of(&l);
        assert_eq!(cp.bytes, l.len() as u64);
        cp.verify(&l).unwrap();
        let mut grown = l.clone();
        grown.extend_from_slice(ledger_line("revoke", "demo", "sha256:a", None, 2).as_bytes());
        cp.verify(&grown).unwrap();
        // Deleted, truncated, same-length rewrite, one byte flipped.
        assert!(cp.verify(b"").unwrap_err().contains("missing or empty"));
        assert!(cp.verify(&l[..l.len() - 1]).unwrap_err().contains("fewer"));
        let mut same = l.clone();
        same[10] ^= 1;
        assert!(cp.verify(&same).unwrap_err().contains("rewritten"));
        // Non-UTF-8 bytes: hashed raw, so a lossy-equal rewrite is caught.
        let mut raw = l.clone();
        raw.push(0xff);
        let cp_raw = LedgerCheckpoint::of(&raw);
        let mut other = l;
        other.push(0xfe);
        assert!(cp_raw.verify(&other).unwrap_err().contains("rewritten"));
        assert_eq!(
            String::from_utf8_lossy(&raw),
            String::from_utf8_lossy(&other)
        );
    }

    /// #879: `save_checkpointed` records the ledger, a ledger that grew
    /// after (a crash before the approvals write) stays valid, and deleting,
    /// truncating or rewriting it is refused with nothing written.
    #[test]
    fn save_checkpointed_refuses_a_ledger_that_lost_history() {
        let ws = tmp("cp-save");
        let (a, path) = demo_approvals(&ws);
        let ledger = ws.join(LEDGER_FILE);
        let l1 = ledger_line("approve", "demo", &a.skills["demo"].hash, None, 5);
        append(&ledger, l1.as_bytes());
        let cp = a.save_checkpointed(&path).unwrap();
        assert_eq!(cp, LedgerCheckpoint::of(l1.as_bytes()));
        let (loaded, got) = Approvals::load_with_checkpoint(&path).unwrap();
        assert_eq!((loaded, got), (a.clone(), Some(cp.clone())));
        verify_checkpoint_at(&path).unwrap();

        // Crash after a ledger append, before the approvals write.
        append(
            &ledger,
            ledger_line("revoke", "demo", "h", None, 6).as_bytes(),
        );
        verify_checkpoint_at(&path).unwrap();
        // The next write moves the checkpoint forward.
        let cp2 = a.save_checkpointed(&path).unwrap();
        assert!(cp2.bytes > cp.bytes);

        let before = std::fs::read(&path).unwrap();
        for bad in [None, Some(&l1.as_bytes()[..10]), Some(&b"{\"x\":1}\n"[..])] {
            match bad {
                None => std::fs::remove_file(&ledger).unwrap(),
                Some(b) => std::fs::write(&ledger, b).unwrap(),
            }
            let e = a.save_checkpointed(&path).unwrap_err();
            assert!(e.contains("tole skills recheckpoint"), "{e}");
            assert_eq!(std::fs::read(&path).unwrap(), before, "nothing written");
            assert!(verify_checkpoint_at(&path).is_err());
        }
    }

    /// #879: `save` (the embedder's write) never drops a checkpoint, a file
    /// without one stays loadable (the pre-#879 format), and a malformed
    /// checkpoint field fails loudly.
    #[test]
    fn save_keeps_the_checkpoint_and_an_old_file_still_loads() {
        let ws = tmp("cp-keep");
        let (mut a, path) = demo_approvals(&ws);
        // Old format: no checkpoint, no ledger: loads, nothing to verify.
        a.save(&path).unwrap();
        assert_eq!(Approvals::load_with_checkpoint(&path).unwrap().1, None);
        verify_checkpoint_at(&path).unwrap();
        // Checkpointed, then an embedder save: the checkpoint stays.
        append(&ws.join(LEDGER_FILE), b"x\n");
        let cp = a.save_checkpointed(&path).unwrap();
        assert!(a.revoke("demo"));
        a.save(&path).unwrap();
        assert_eq!(
            Approvals::load_with_checkpoint(&path).unwrap(),
            (a.clone(), Some(cp))
        );
        // Wrong shape: an error, not "no checkpoint".
        let raw = std::fs::read_to_string(&path).unwrap();
        let broken = raw.replace("\"bytes\": 2", "\"bytes\": \"two\"");
        assert_ne!(raw, broken);
        std::fs::write(&path, broken).unwrap();
        assert!(Approvals::load_with_checkpoint(&path)
            .unwrap_err()
            .contains("malformed"));
    }

    /// #879: the owner's recovery records exactly the ledger they reviewed
    /// and refuses one that changed since.
    #[test]
    fn accept_ledger_records_the_reviewed_ledger_only() {
        let ws = tmp("cp-accept");
        let (a, path) = demo_approvals(&ws);
        let ledger = ws.join(LEDGER_FILE);
        append(&ledger, b"line one\n");
        a.save_checkpointed(&path).unwrap();
        std::fs::remove_file(&ledger).unwrap();
        assert!(verify_checkpoint_at(&path).is_err());
        let reviewed = LedgerCheckpoint::of(&read_ledger_bytes(&path).unwrap());
        append(&ledger, b"changed meanwhile\n");
        let e = Approvals::accept_ledger(&path, &reviewed).unwrap_err();
        assert!(e.contains("changed while"), "{e}");
        assert!(verify_checkpoint_at(&path).is_err());
        let reviewed = LedgerCheckpoint::of(&read_ledger_bytes(&path).unwrap());
        Approvals::accept_ledger(&path, &reviewed).unwrap();
        verify_checkpoint_at(&path).unwrap();
        assert_eq!(Approvals::load(&path).unwrap(), a, "approvals untouched");
    }

    /// #879: an over-cap ledger is refused before anything is hashed, and
    /// an adopted line reads as an approval with its note.
    #[test]
    fn the_ledger_cap_bounds_the_hashed_read_and_adopted_lines_approve() {
        let ws = tmp("cp-cap");
        let path = ws.join("approvals.json");
        let f = std::fs::File::create(ws.join(LEDGER_FILE)).unwrap();
        f.set_len(MAX_LEDGER_BYTES + 1).unwrap();
        assert!(read_ledger_bytes(&path).unwrap_err().contains("16 MiB"));
        let line = adopted_ledger_line("demo", "sha256:a", None, 9);
        assert!(line.contains("\"note\":\"adopted\"") && line.ends_with('\n'));
        let mut a = Approvals::default();
        a.skills.insert(
            "demo".into(),
            Approval {
                hash: "sha256:a".into(),
                approved_at: 1,
                files: 1,
                source: None,
            },
        );
        assert_eq!(check_against_ledger(&a, &line), LedgerCheck::default());
        assert_eq!(ledger_history(&line, "demo", 1)[0].1, "approve");
    }
}
