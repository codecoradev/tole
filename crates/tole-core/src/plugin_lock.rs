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

/// Current lock format version; a newer one is refused.
pub const LOCK_VERSION: u32 = 1;
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
    /// HTTP); recorded, verified by `tole plugin` (part 2).
    #[serde(default)]
    pub integrity: String,
    /// [`manifest_hash`] of the approved tool list.
    pub manifest_hash: String,
    /// Secret NAMES granted to this server.
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
        let json = serde_json::to_vec_pretty(self).map_err(|e| e.to_string())?;
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
    /// Locked, not revoked, and the live manifest and version match.
    Trusted,
    /// No lock entry.
    Unlocked,
    /// Revoked (or a project pin conflicts).
    Revoked,
    /// The live manifest or version differs from the approval.
    Changed { locked: String, live: String },
}

/// Decides whether `server`'s live tools (and reported version) may be
/// registered under `lock`.
pub fn admit(
    lock: &BTreeMap<String, LockEntry>,
    server: &str,
    version: &str,
    tools: &[ManifestTool],
) -> Admission {
    let Some(e) = lock.get(server) else {
        return Admission::Unlocked;
    };
    if e.revoked {
        return Admission::Revoked;
    }
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
