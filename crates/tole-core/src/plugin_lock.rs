//! Plugin lockfile and tool-manifest hash (#599 part 1, design
//! `third-party-plugins-design.md` §4.1–4.2): the rug-pull guard.
//!
//! An MCP server's tool names, descriptions and input schemas are
//! model-visible instructions, and a server can change them after it was
//! approved. So the owner's approval pins a **manifest hash** over all of
//! them, and every connect re-hashes the live list: a mismatch, a revoked
//! entry or a server with no entry is never registered silently.
//!
//! - [`manifest_hash`] is SHA-256 over a canonical, length-prefixed
//!   encoding (tools sorted by name; each field prefixed by its byte
//!   length; schemas as canonical JSON with sorted keys), so moving text
//!   from one field or tool to another always changes the hash.
//! - A [`Lockfile`] is versioned JSON: the user lock in the tole data dir
//!   (`plugins.lock`) and the project lock `.tole/plugins.lock`. It is read
//!   without following a symlink or blocking, with a size cap; an
//!   unreadable or malformed lock is an error (fail closed).
//! - [`effective`] layers them: trust comes from the USER lock only. A
//!   project entry can only narrow it (pin the same hash, drop secrets,
//!   revoke); it never trusts a server or grants a secret on its own.
//! - [`admit`] is the connect-time decision. Anything but an exact match
//!   is refused; a hash mismatch never fails open.
//! - [`ledger_line`] is the append-only audit record of an approval, a
//!   manifest change or a revocation.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// Current lock format version; a newer one is refused. Version 2 adds the
/// per-tool manifest snapshot (`manifest`); version 1 entries load without
/// one.
pub const LOCK_VERSION: u32 = 2;
/// Text a lock entry's snapshot may hold for one server, all tools
/// together; past it a tool keeps only its hashes.
pub const SNAPSHOT_BUDGET: usize = 128 * 1024;
/// Most characters of one description or schema kept in a snapshot.
pub const SNAPSHOT_FIELD_CAP: usize = 8 * 1024;
/// What a snapshot holds instead of text it did not store.
pub const NOT_STORED: &str = "«not stored in the lock»";
/// The lock file name (user: in the tole data dir; project: under `.tole/`).
pub const LOCK_FILE: &str = "plugins.lock";
/// The plugin audit ledger in the tole data dir.
pub const LEDGER_FILE: &str = "plugin-ledger.jsonl";
/// Largest lock file read.
pub const MAX_LOCK_BYTES: u64 = 1024 * 1024;

/// One tool as the server lists it.
#[derive(Debug, Clone, PartialEq)]
pub struct ManifestTool {
    pub name: String,
    pub description: String,
    pub input_schema: Value,
}

/// JSON with object keys sorted at every depth (independent of how
/// `serde_json` orders maps in this build).
pub fn canonical_json(v: &Value) -> String {
    match v {
        Value::Object(m) => {
            let mut keys: Vec<&String> = m.keys().collect();
            keys.sort();
            let body: Vec<String> = keys
                .into_iter()
                .map(|k| format!("{}:{}", Value::String(k.clone()), canonical_json(&m[k])))
                .collect();
            format!("{{{}}}", body.join(","))
        }
        Value::Array(a) => format!(
            "[{}]",
            a.iter().map(canonical_json).collect::<Vec<_>>().join(",")
        ),
        other => other.to_string(),
    }
}

/// `sha256:<hex>` of a server's tool manifest (see the module docs).
pub fn manifest_hash(tools: &[ManifestTool]) -> String {
    let mut sorted: Vec<&ManifestTool> = tools.iter().collect();
    sorted.sort_by(|a, b| a.name.cmp(&b.name));
    let mut h = Sha256::new();
    h.update(b"tole-mcp-manifest-v1\0");
    h.update((sorted.len() as u64).to_be_bytes());
    for t in sorted {
        for field in [
            t.name.as_bytes(),
            t.description.as_bytes(),
            canonical_json(&t.input_schema).as_bytes(),
        ] {
            h.update((field.len() as u64).to_be_bytes());
            h.update(field);
        }
    }
    let hex: String = h.finalize().iter().map(|b| format!("{b:02x}")).collect();
    format!("sha256:{hex}")
}

/// `sha256:<hex>` of a file's bytes, read in chunks (`None` when it
/// cannot be read).
pub fn file_sha256(path: &Path) -> Option<String> {
    use std::io::Read;
    let mut f = std::fs::File::open(path).ok()?;
    let mut h = Sha256::new();
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        match f.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => h.update(&buf[..n]),
            Err(_) => return None,
        }
    }
    let hex: String = h.finalize().iter().map(|b| format!("{b:02x}")).collect();
    Some(format!("sha256:{hex}"))
}

/// Terminal-safe text: control characters (newline and tab kept), bidi
/// overrides and zero-width characters dropped. Never shortened.
pub fn terminal_safe(s: &str) -> String {
    s.chars()
        .filter(|c| {
            (!c.is_control() || matches!(c, '\n' | '\t'))
                && !matches!(*c, '\u{200B}'..='\u{200F}' | '\u{202A}'..='\u{202E}'
                    | '\u{2066}'..='\u{2069}' | '\u{FEFF}')
        })
        .collect()
}

fn sha(bytes: &[u8]) -> String {
    let hex: String = Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    format!("sha256:{hex}")
}

/// One tool as the lock remembers it: exact hashes of its description and
/// schema, and their text redacted, made terminal-safe and capped (in that
/// order), for `tole plugin diff`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SnapshotTool {
    pub name: String,
    pub description_hash: String,
    pub schema_hash: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub schema: String,
}

fn shown(text: &str, secrets: &crate::secrets::SecretSet, budget: &mut usize) -> String {
    let safe = terminal_safe(&secrets.redact(text));
    let cap = SNAPSHOT_FIELD_CAP.min(*budget);
    let n = safe.chars().count();
    if n <= cap {
        *budget -= n;
        return safe;
    }
    if cap < 64 {
        return NOT_STORED.to_string();
    }
    *budget -= cap;
    let mut cut: String = safe.chars().take(cap).collect();
    cut.push_str("…[cut]");
    cut
}

/// The snapshot of `tools` stored with an approval (sorted by name).
pub fn snapshot(tools: &[ManifestTool], secrets: &crate::secrets::SecretSet) -> Vec<SnapshotTool> {
    let mut sorted: Vec<&ManifestTool> = tools.iter().collect();
    sorted.sort_by(|a, b| a.name.cmp(&b.name));
    let mut budget = SNAPSHOT_BUDGET;
    sorted
        .into_iter()
        .map(|t| {
            let schema = canonical_json(&t.input_schema);
            SnapshotTool {
                name: terminal_safe(&secrets.redact(&t.name)),
                description_hash: sha(t.description.as_bytes()),
                schema_hash: sha(schema.as_bytes()),
                description: shown(&t.description, secrets, &mut budget),
                schema: shown(&schema, secrets, &mut budget),
            }
        })
        .collect()
}

/// One difference between a locked snapshot and the live tool list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToolChange {
    Added(SnapshotTool),
    Removed(SnapshotTool),
    Description {
        old: SnapshotTool,
        new: SnapshotTool,
    },
    Schema {
        old: SnapshotTool,
        new: SnapshotTool,
    },
}

/// Tool by tool, what changed from the locked snapshot `old` to `live`
/// (compared by exact hashes, so a change past the stored text still
/// shows).
pub fn tool_changes(
    old: &[SnapshotTool],
    live: &[ManifestTool],
    secrets: &crate::secrets::SecretSet,
) -> Vec<ToolChange> {
    let new = snapshot(live, secrets);
    let find = |set: &[SnapshotTool], name: &str| set.iter().find(|t| t.name == name).cloned();
    let mut out = Vec::new();
    for n in &new {
        match find(old, &n.name) {
            None => out.push(ToolChange::Added(n.clone())),
            Some(o) => {
                if o.description_hash != n.description_hash {
                    out.push(ToolChange::Description {
                        old: o.clone(),
                        new: n.clone(),
                    });
                }
                if o.schema_hash != n.schema_hash {
                    out.push(ToolChange::Schema {
                        old: o,
                        new: n.clone(),
                    });
                }
            }
        }
    }
    for o in old {
        if find(&new, &o.name).is_none() {
            out.push(ToolChange::Removed(o.clone()));
        }
    }
    out
}

/// One approved server.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LockEntry {
    /// Where it comes from: `stdio:<command line>` or the HTTP url.
    pub source: String,
    /// The version the server reported at approval (`serverInfo.version`).
    #[serde(default)]
    pub version: String,
    /// `sha256:<hex>` of the resolved stdio binary at approval (empty for
    /// HTTP or an unresolvable program). Checked at every connect (#679):
    /// a different binary at the same path is not admitted.
    #[serde(default)]
    pub integrity: String,
    /// [`manifest_hash`] of the approved tool list.
    pub manifest_hash: String,
    /// Secret NAMES granted to this server: the `mcp` grants held when it
    /// was approved. Its stdio process receives only these, and only while
    /// the `[secrets]` release still binds them to `mcp` (#679); a project
    /// lock can drop names.
    #[serde(default)]
    pub secrets: Vec<String>,
    /// Sandbox profile (recorded; enforcement is a later part).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sandbox: Option<String>,
    /// Seconds since the Unix epoch.
    #[serde(default)]
    pub approved_at: u64,
    /// Revoked entries stay (append-only intent): never connected again.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub revoked: bool,
    /// The approved tools, for `tole plugin diff` (empty in a version 1
    /// entry: then only the hash is known).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub manifest: Vec<SnapshotTool>,
}

/// A lock file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Lockfile {
    pub version: u32,
    #[serde(default)]
    pub servers: BTreeMap<String, LockEntry>,
}

impl Default for Lockfile {
    fn default() -> Self {
        Self {
            version: LOCK_VERSION,
            servers: BTreeMap::new(),
        }
    }
}

impl Lockfile {
    /// `<tole data dir>/plugins.lock`.
    pub fn user_path() -> Option<PathBuf> {
        crate::paths::tole_data_dir().map(|d| d.join(LOCK_FILE))
    }

    /// `<workspace>/.tole/plugins.lock`.
    pub fn project_path(workspace: &Path) -> PathBuf {
        workspace.join(".tole").join(LOCK_FILE)
    }

    /// Reads `path`: missing is an empty lock; anything unreadable,
    /// oversized, malformed or of a newer version is an error.
    pub fn load(path: &Path) -> Result<Self, String> {
        let raw = match read_lock_bytes(path) {
            Ok(r) => r,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Self::default()),
            Err(e) => return Err(format!("{}: {e}", path.display())),
        };
        let lock: Self = serde_json::from_slice(&raw)
            .map_err(|e| format!("{}: malformed plugin lock: {e}", path.display()))?;
        if lock.version > LOCK_VERSION {
            return Err(format!(
                "{}: plugin lock version {} is newer than this build reads ({LOCK_VERSION}); \
                 upgrade tole",
                path.display(),
                lock.version
            ));
        }
        Ok(lock)
    }

    /// Writes `path` atomically (temp file + rename), creating its directory.
    pub fn save(&self, path: &Path) -> Result<(), String> {
        let dir = path.parent().unwrap_or_else(|| Path::new("."));
        std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
        let current = Self {
            version: LOCK_VERSION,
            servers: self.servers.clone(),
        };
        let json = serde_json::to_vec_pretty(&current).map_err(|e| e.to_string())?;
        let tmp = dir.join(format!(".{LOCK_FILE}.{}.tmp", std::process::id()));
        std::fs::write(&tmp, json).map_err(|e| format!("{}: {e}", tmp.display()))?;
        std::fs::rename(&tmp, path).map_err(|e| {
            let _ = std::fs::remove_file(&tmp);
            format!("{}: {e}", path.display())
        })
    }
}

/// The lock's bytes: no final symlink followed, never blocking (a FIFO is
/// refused on the open handle), at most [`MAX_LOCK_BYTES`].
fn read_lock_bytes(path: &Path) -> std::io::Result<Vec<u8>> {
    use std::io::Read;
    let mut o = std::fs::OpenOptions::new();
    o.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        o.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    let f = o.open(path)?;
    let meta = f.metadata()?;
    if !meta.is_file() {
        return Err(std::io::Error::other("not a regular file"));
    }
    let mut buf = Vec::new();
    f.take(MAX_LOCK_BYTES + 1).read_to_end(&mut buf)?;
    if meta.len() > MAX_LOCK_BYTES || buf.len() as u64 > MAX_LOCK_BYTES {
        return Err(std::io::Error::other(format!(
            "larger than {MAX_LOCK_BYTES} bytes"
        )));
    }
    Ok(buf)
}

/// The layered lock: the user lock's entries, each narrowed by a project
/// entry of the same name. A project entry can revoke, keep only secrets
/// the user granted, and pin a hash; a pin that differs from the user's
/// approval makes the server untrusted (`revoked`) until both agree. A
/// project entry for a server the user never approved is ignored: a cloned
/// repository cannot trust a server. Returns the notes for ignored or
/// conflicting project entries.
pub fn effective(
    user: &Lockfile,
    project: &Lockfile,
) -> (BTreeMap<String, LockEntry>, Vec<String>) {
    let mut out = user.servers.clone();
    let mut notes = Vec::new();
    for (name, p) in &project.servers {
        let Some(u) = out.get_mut(name) else {
            notes.push(format!(
                "plugin lock: the project lock names `{name}`, which your user lock does not \
                 approve; a project lock cannot trust a server (approve it yourself first)"
            ));
            continue;
        };
        u.secrets.retain(|s| p.secrets.contains(s));
        if p.revoked {
            u.revoked = true;
        }
        if p.manifest_hash != u.manifest_hash {
            u.revoked = true;
            notes.push(format!(
                "plugin lock: the project lock pins `{name}` to another manifest than your \
                 approval; it stays unavailable until both locks agree"
            ));
        }
    }
    (out, notes)
}

/// The connect-time decision for one server.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Admission {
    /// Locked, not revoked, and the live source, binary, manifest and
    /// version match.
    Trusted,
    /// No lock entry.
    Unlocked,
    /// Revoked (or a project pin conflicts).
    Revoked,
    /// The live manifest or version differs from the approval.
    Changed { locked: String, live: String },
    /// The configured command line or URL is not the approved one (#679):
    /// the name now points at another server, whatever it reports.
    Moved,
    /// The stdio program's file is not the approved binary (#679).
    Rebuilt,
}

/// What a connect sees of a server, compared against its lock entry.
#[derive(Debug, Clone, Copy)]
pub struct Live<'a> {
    /// `stdio:<command line>` or the url, as configured now.
    pub source: &'a str,
    /// `sha256:<hex>` of the stdio program as it resolves now (empty when it
    /// cannot be read, or for HTTP).
    pub integrity: &'a str,
    /// The version the server reports.
    pub version: &'a str,
    /// The tools the server lists.
    pub tools: &'a [ManifestTool],
}

/// Decides whether `server` may be registered under `lock`: its source and
/// binary (the host's own facts, #679), then its manifest and version (the
/// server's claims, which any process can replay).
pub fn admit(lock: &BTreeMap<String, LockEntry>, server: &str, live: &Live<'_>) -> Admission {
    let Some(e) = lock.get(server) else {
        return Admission::Unlocked;
    };
    if e.revoked {
        return Admission::Revoked;
    }
    if e.source != live.source {
        return Admission::Moved;
    }
    // An approval that recorded a binary hash binds it; one that could not
    // (HTTP, an unresolvable program) has nothing to compare.
    if !e.integrity.is_empty() && e.integrity != live.integrity {
        return Admission::Rebuilt;
    }
    let (version, tools) = (live.version, live.tools);
    let live = manifest_hash(tools);
    if live != e.manifest_hash || version != e.version {
        return Admission::Changed {
            locked: format!("{} {}", e.version, e.manifest_hash),
            live: format!("{version} {live}"),
        };
    }
    Admission::Trusted
}

/// One audit-ledger line (`approve`, `manifest_change`, `revoke`), names and
/// hashes only.
pub fn ledger_line(event: &str, server: &str, manifest_hash: &str, at: u64) -> String {
    let mut line = serde_json::json!({
        "v": 1, "kind": "plugin", "event": event, "server": server,
        "manifest_hash": manifest_hash, "at": at,
    })
    .to_string();
    line.push('\n');
    line
}

#[cfg(test)]
#[path = "plugin_lock_tests.rs"]
mod tests;
