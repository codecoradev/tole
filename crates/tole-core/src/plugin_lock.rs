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
//! - A [`BundleRef`] (#811) binds an entry to the bundle manifest
//!   (`tole-plugin.toml`) it was approved from: [`bundle_admission`] refuses
//!   the server when that file's hash is no longer the approved one, and
//!   [`secret_ledger_line`] records each secret grant and revoke.
//! - The sandbox profile an entry was approved in (#750) is recorded by name
//!   and by [`crate::sandbox::identity::ProfileId`] hash (workspace, network,
//!   resolved read and write grants): [`profile_admission`] refuses the
//!   server when the profile it would run in now is another one, and
//!   [`profile_ledger_line`] records it so the ledger, not the lock file,
//!   is the authority.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// Current lock format version; a newer one is refused. Version 2 adds the
/// per-tool manifest snapshot (`manifest`); version 1 entries load without
/// one. Version 3 adds the bundle record (`bundle`, #811). Version 4 adds
/// the hash of the sandbox profile (`sandbox_hash`, #750); an entry without
/// one loads and is reported, not blocked.
pub const LOCK_VERSION: u32 = 4;
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

/// `sha256:<hex>` of `bytes`.
pub fn sha(bytes: &[u8]) -> String {
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

/// How far an MCP server is trusted (design §4.3). Data in the lock entry,
/// shown by `tole plugin list`; what each tier may do is enforced where the
/// server is started (sandbox, interactive only), not by this label.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Tier {
    /// The owner's own servers (the first-party list): lock, manifest hash,
    /// scrubbed environment.
    FirstParty,
    /// A third-party server whose namespace the official registry verified
    /// (set only by registry resolution, a later part).
    ThirdPartyVerified,
    /// Anything else: interactive sessions only, sandbox required.
    ThirdPartyUnverified,
}

impl Tier {
    pub fn as_str(self) -> &'static str {
        match self {
            Tier::FirstParty => "first-party",
            Tier::ThirdPartyVerified => "third-party-verified",
            Tier::ThirdPartyUnverified => "third-party-unverified",
        }
    }
}

/// The servers that count as first-party until the owner configures
/// `[plugins] first_party` (design §4.3).
pub const DEFAULT_FIRST_PARTY: [&str; 4] = ["cora", "uteke", "nginjen", "laya"];

/// The tier of server `name`. A first-party claim holds only for a name on
/// the owner's `first_party` list, whatever a lock entry says (the lock is a
/// file, the list is owner config); the stored tier can otherwise only be
/// third-party, and an entry without one is unverified.
pub fn tier_of(name: &str, entry: Option<&LockEntry>, first_party: &[String]) -> Tier {
    if first_party.iter().any(|f| f == name) {
        return Tier::FirstParty;
    }
    match entry.and_then(|e| e.tier) {
        Some(Tier::ThirdPartyVerified) => Tier::ThirdPartyVerified,
        _ => Tier::ThirdPartyUnverified,
    }
}

/// The bundle manifest an entry was approved from (#811).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BundleRef {
    /// The plugin name the manifest declares.
    pub plugin: String,
    /// The version the manifest declares.
    #[serde(default)]
    pub version: String,
    /// `sha256:<hex>` of the manifest file's bytes at approval.
    pub hash: String,
    /// Absolute path the manifest was read from; it is re-hashed before the
    /// server is started.
    pub path: String,
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
    /// Name of the sandbox profile the server was approved in (`None`: it
    /// was not sandboxed then). With [`LockEntry::sandbox_hash`] it is
    /// compared with the profile it would run in at every connect (#750).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sandbox: Option<String>,
    /// [`crate::sandbox::identity::ProfileId`] hash of that profile's
    /// effective grants (lock version 4). `None` in an older entry or one
    /// approved unsandboxed: nothing to compare.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sandbox_hash: Option<String>,
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
    /// The tier recorded at approval (see [`tier_of`] for how it is read).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tier: Option<Tier>,
    /// The bundle manifest this server was approved from, if any. The
    /// `secrets` of such an entry are bound to it: they were approved one by
    /// one for exactly this manifest hash.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bundle: Option<BundleRef>,
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
    read_bounded(path, MAX_LOCK_BYTES)
}

/// [`read_lock_bytes`] with another size cap.
fn read_bounded(path: &Path, max: u64) -> std::io::Result<Vec<u8>> {
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
    f.take(max + 1).read_to_end(&mut buf)?;
    if meta.len() > max || buf.len() as u64 > max {
        return Err(std::io::Error::other(format!("larger than {max} bytes")));
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
        // A project lock can pin the profile too, but never replace it: a
        // pin that differs from the user's approval makes the server
        // unavailable, and a pin of nothing changes nothing.
        if (p.sandbox_hash.is_some() && p.sandbox_hash != u.sandbox_hash)
            || (p.sandbox.is_some() && p.sandbox != u.sandbox)
        {
            u.revoked = true;
            notes.push(format!(
                "plugin lock: the project lock pins `{name}` to another sandbox profile than \
                 your approval; it stays unavailable until both locks agree"
            ));
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
    /// The bundle manifest the server was approved from changed, or cannot
    /// be read now (#811): its secret grants no longer apply to what is on
    /// disk.
    BundleChanged,
    /// The sandbox profile the server would run in now (name or effective
    /// grants) is not the one it was approved in, or the sandbox no longer
    /// applies to it (#750).
    ProfileChanged,
}

/// The sandbox half of the connect-time decision: `None` when `server` has
/// no recorded profile hash (approved before it was recorded, or
/// unsandboxed: nothing to compare), or `live` is exactly the approved name
/// and hash; otherwise [`Admission::ProfileChanged`]. `live` is `None` when
/// no profile applies to the server now (sandbox off or skipped), which is
/// a loosening and not admitted. Compared exactly: a tighter profile needs
/// the owner's approval too, it is simply the one that shows what changed.
pub fn profile_admission(
    lock: &BTreeMap<String, LockEntry>,
    server: &str,
    live: Option<&crate::sandbox::identity::ProfileId>,
) -> Option<Admission> {
    let e = lock.get(server)?;
    let hash = e.sandbox_hash.as_deref()?;
    let same =
        live.is_some_and(|l| l.hash == hash && Some(l.name.as_str()) == e.sandbox.as_deref());
    (!same).then_some(Admission::ProfileChanged)
}

/// The bundle half of the connect-time decision: `None` when `server` has no
/// bundle record, or the manifest file hashes to the approved hash;
/// otherwise [`Admission::BundleChanged`]. `live` is the hash of the file at
/// the recorded path now, empty when it cannot be read (fail closed). A
/// manifest hash is compared exactly: any byte change needs a new approval.
pub fn bundle_admission(
    lock: &BTreeMap<String, LockEntry>,
    server: &str,
    live: &str,
) -> Option<Admission> {
    let b = lock.get(server)?.bundle.as_ref()?;
    (live.is_empty() || live != b.hash).then_some(Admission::BundleChanged)
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

/// What the lock decides about a configured server BEFORE it is started:
/// `None` when the approved launch identity (command line or URL, program
/// file) matches and the server may be started to compare its tools;
/// otherwise why it must not be (never `Trusted` or `Changed`: those need
/// the live tool list).
pub fn admit_unseen(
    lock: &BTreeMap<String, LockEntry>,
    server: &str,
    source: &str,
    integrity: &str,
) -> Option<Admission> {
    let Some(e) = lock.get(server) else {
        return Some(Admission::Unlocked);
    };
    if e.revoked {
        return Some(Admission::Revoked);
    }
    if e.source != source {
        return Some(Admission::Moved);
    }
    if !e.integrity.is_empty() && e.integrity != integrity {
        return Some(Admission::Rebuilt);
    }
    None
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

/// The audit line of a bundle approval: [`ledger_line`] plus the hash of the
/// manifest file it was approved from.
pub fn bundle_ledger_line(
    event: &str,
    server: &str,
    manifest_hash: &str,
    bundle_hash: &str,
    at: u64,
) -> String {
    let mut line = serde_json::json!({
        "v": 1, "kind": "plugin", "event": event, "server": server,
        "manifest_hash": manifest_hash, "bundle_hash": bundle_hash, "at": at,
    })
    .to_string();
    line.push('\n');
    line
}

/// The audit line of a plain (non-bundle) approval: [`ledger_line`] plus a
/// marker that this approval's secret grants are recorded as `secret_grant`
/// lines right after it (#825), even when there are none. Without the marker
/// an `approve` line is an older one, whose entry's `secrets` (#679) the
/// ledger cannot vouch for.
pub fn plain_ledger_line(server: &str, manifest_hash: &str, at: u64) -> String {
    let mut line = serde_json::json!({
        "v": 1, "kind": "plugin", "event": "approve", "server": server,
        "manifest_hash": manifest_hash, "secrets_recorded": true, "at": at,
    })
    .to_string();
    line.push('\n');
    line
}

/// The audit line that follows every approval (#750): the sandbox profile
/// the entry was approved in, `None` when it was not sandboxed. Without such
/// a line after the last `approve`, the ledger has no record of the profile
/// (an older approval) and only the lock knows it.
pub fn profile_ledger_line(
    server: &str,
    name: Option<&str>,
    hash: Option<&str>,
    at: u64,
) -> String {
    let mut line = serde_json::json!({
        "v": 1, "kind": "plugin", "event": "sandbox_profile", "server": server,
        "profile": name, "profile_hash": hash, "at": at,
    })
    .to_string();
    line.push('\n');
    line
}

/// The audit line of one secret grant or revoke (`secret_grant`,
/// `secret_revoke`): the secret's NAME, never a value.
pub fn secret_ledger_line(event: &str, server: &str, secret: &str, at: u64) -> String {
    let mut line = serde_json::json!({
        "v": 1, "kind": "plugin", "event": event, "server": server,
        "secret": secret, "at": at,
    })
    .to_string();
    line.push('\n');
    line
}

/// Largest ledger read for [`check_against_ledger`].
pub const MAX_LEDGER_BYTES: u64 = 64 * 1024 * 1024;

/// The ledger text. `Ok(None)`: there is no ledger (nothing to compare).
/// `Err`: it exists but cannot be read (a symlink or other non-file, an
/// I/O error, larger than [`MAX_LEDGER_BYTES`]): the comparison cannot be
/// made, and the caller must say so rather than carry on as if it had.
pub fn read_ledger(path: &Path) -> Result<Option<String>, String> {
    match read_bounded(path, MAX_LEDGER_BYTES) {
        Ok(raw) => Ok(Some(String::from_utf8_lossy(&raw).into_owned())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(format!("{}: {e}", path.display())),
    }
}

/// Whether the ledger holds `server`'s current secret grants (#825): its
/// last approval recorded them (a bundle approval, or a plain one with the
/// `secrets_recorded` marker) or a `secret_grant` line followed it. `false`
/// for an older approval, whose grants only the lock knows.
pub fn secrets_recorded(ledger: &str, server: &str) -> bool {
    let mut tracked = false;
    for line in ledger.lines() {
        let Ok(v) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        if v["kind"] != "plugin" || v["server"] != server {
            continue;
        }
        match v["event"].as_str() {
            Some("approve") => {
                tracked = v["bundle_hash"].is_string() || v["secrets_recorded"] == true;
            }
            Some("revoke") => tracked = false,
            Some("secret_grant") => tracked = true,
            _ => {}
        }
    }
    tracked
}

/// The sandbox profile `(name, hash)` the ledger recorded for `server`'s
/// current approval: `None` when the last decision is a revoke, there is no
/// approval, or no `sandbox_profile` line followed it (an older approval).
pub fn ledger_profile(ledger: &str, server: &str) -> Option<(Option<String>, Option<String>)> {
    let mut rec = None;
    for line in ledger.lines() {
        let Ok(v) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        if v["kind"] != "plugin" || v["server"] != server {
            continue;
        }
        match v["event"].as_str() {
            Some("approve") | Some("revoke") => rec = None,
            Some("sandbox_profile") => {
                let s = |k: &str| v[k].as_str().map(str::to_string);
                rec = Some((s("profile"), s("profile_hash")));
            }
            _ => {}
        }
    }
    rec
}

/// The ledger's last decision for `server` as `(revoked, profile)`: whether it
/// is a `revoke`, and the `(name, hash)` the last approval recorded. Unlike
/// [`ledger_profile`], a `revoke` keeps the profile (it only says the server
/// is revoked now); a new `approve` clears it until its own profile line.
type LastDecision = (bool, Option<(Option<String>, Option<String>)>);

fn ledger_last_decision(ledger: &str, server: &str) -> LastDecision {
    let (mut revoked, mut rec) = (false, None);
    for line in ledger.lines() {
        let Ok(v) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        if v["kind"] != "plugin" || v["server"] != server {
            continue;
        }
        match v["event"].as_str() {
            Some("approve") => (revoked, rec) = (false, None),
            Some("revoke") => revoked = true,
            Some("sandbox_profile") => {
                let s = |k: &str| v[k].as_str().map(str::to_string);
                rec = Some((s("profile"), s("profile_hash")));
            }
            _ => {}
        }
    }
    (revoked, rec)
}

/// The bundle hash the ledger recorded on `server`'s LAST `approve` line:
/// `None` when the ledger holds no approval of it, `Some(None)` when the last
/// approval was a plain one (no bundle), `Some(Some(hash))` for a bundle
/// approval. A `revoke` does not reset it: a revoked bundle server is still
/// a bundle server, so a lock that drops the record cannot turn it plain.
pub fn ledger_bundle(ledger: &str, server: &str) -> Option<Option<String>> {
    let mut rec = None;
    for line in ledger.lines() {
        let Ok(v) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        if v["kind"] == "plugin" && v["server"] == server && v["event"] == "approve" {
            rec = Some(v["bundle_hash"].as_str().map(str::to_string));
        }
    }
    rec
}

/// Whether the sandbox profile of `server` moved since its approval, decided
/// by the ledger and never by the lock file alone. This is what a script may
/// not approve past; only a terminal can.
///
/// - **Revoked and sandboxed:** the ledger's last decision is a `revoke` and
///   the approval it revoked recorded a sandbox profile: moved, whatever the
///   lock says (entry kept, edited or deleted) and whatever the live profile
///   is. Only a terminal re-approves it. A revoked server that ran
///   unsandboxed promised nothing and stays re-approvable by a script.
/// - **No lock entry** (never approved, or deleted by hand): the ledger's
///   recorded profile hash alone is compared with the live profile; no record
///   or no hash admits any.
/// - **With an entry:** a lock record that differs from the ledger's, or a
///   lock hash the ledger never recorded, is itself a move (the lock was
///   edited, exactly what [`check_against_ledger`] blocks). With a record in
///   the ledger, the live profile must equal it; one that recorded no sandbox
///   admits any. Without a ledger (`None`) or an approval with no profile
///   line, only the lock's own record exists and [`profile_admission`]
///   decides (the migration case).
pub fn profile_moved(
    lock: &BTreeMap<String, LockEntry>,
    ledger: Option<&str>,
    server: &str,
    live: Option<&crate::sandbox::identity::ProfileId>,
) -> bool {
    let same = |name: &Option<String>, h: &str| {
        live.is_some_and(|l| l.hash == h && Some(l.name.as_str()) == name.as_deref())
    };
    let last = ledger.map(|text| ledger_last_decision(text, server));
    if let Some((true, Some((_, Some(_))))) = &last {
        return true;
    }
    let Some(e) = lock.get(server) else {
        return match last {
            Some((_, Some((name, Some(h))))) => !same(&name, &h),
            _ => false,
        };
    };
    if let Some(text) = ledger {
        match ledger_profile(text, server) {
            Some((name, hash)) => {
                if name != e.sandbox || hash != e.sandbox_hash {
                    return true;
                }
                return hash.is_some_and(|h| !same(&name, &h));
            }
            None if e.sandbox_hash.is_some() => return true,
            None => {}
        }
    }
    profile_admission(lock, server, live).is_some()
}

/// What the owner's record of approvals says against the lock file.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LedgerCheck {
    /// Servers the lock shows as trusted but the ledger does not allow, with
    /// why: the last recorded decision is a revoke (the lock was edited back),
    /// or the lock's manifest hash is not the last approved one. They are
    /// treated as revoked until the owner approves them again.
    pub blocked: Vec<(String, String)>,
    /// Trusted entries the ledger has no approval for at all (written by
    /// hand, or approved before the ledger): reported, not blocked.
    pub unrecorded: Vec<String>,
    /// Trusted entries holding secrets the ledger cannot vouch for: approved
    /// before approvals recorded their grants (#825), with no grant line
    /// since. Reported, not blocked (a one-time migration); the owner's next
    /// approval, or `revoke --secret`, records them.
    pub secrets_unrecorded: Vec<String>,
    /// Trusted stdio entries whose approval has no `sandbox_profile` line
    /// (approved before #750): their profile is not compared at connect.
    /// Reported, not blocked (a one-time migration); the owner's next
    /// approval records it.
    pub profile_unrecorded: Vec<String>,
    /// Why the ledger could not be read, when it could not: revocations
    /// recorded there are then not checked.
    pub unreadable: Option<String>,
}

/// Compares the lock's trusted entries with the last `approve` / `revoke`
/// line the ledger holds for each server. Detection, not prevention: the
/// ledger and the lock are both files of the owner, so a process that can
/// rewrite both defeats it; an edit of the lock alone is caught.
pub fn check_against_ledger(lock: &BTreeMap<String, LockEntry>, ledger: &str) -> LedgerCheck {
    // Per server: the last approve/revoke (event, manifest hash, bundle
    // hash) and the secrets the ledger shows granted since that approval.
    type Last = (String, String, Option<String>);
    let mut last: BTreeMap<String, Last> = BTreeMap::new();
    let mut granted: BTreeMap<String, std::collections::BTreeSet<String>> = BTreeMap::new();
    // Servers whose current approval has its grants in the ledger: a bundle
    // approval, a plain one that says so, or one with a grant line since. An
    // older plain approval is not (the migration case, #825).
    let mut tracked: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    // Per server: the sandbox profile (name, hash) the current approval
    // recorded; absent until a `sandbox_profile` line follows the approval.
    type Profile = (Option<String>, Option<String>);
    let mut profiles: BTreeMap<String, Profile> = BTreeMap::new();
    for line in ledger.lines() {
        let Ok(v) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        if v["kind"] != "plugin" {
            continue;
        }
        let (Some(event), Some(server)) = (v["event"].as_str(), v["server"].as_str()) else {
            continue;
        };
        match event {
            "approve" | "revoke" => {
                let hash = v["manifest_hash"].as_str().unwrap_or_default();
                let bundle = v["bundle_hash"].as_str().map(str::to_string);
                let recorded =
                    event == "approve" && (bundle.is_some() || v["secrets_recorded"] == true);
                last.insert(server.to_string(), (event.into(), hash.into(), bundle));
                // An approval starts a new grant set: grants never carry over.
                granted.remove(server);
                tracked.remove(server);
                profiles.remove(server);
                if recorded {
                    tracked.insert(server.to_string());
                }
            }
            "sandbox_profile" => {
                let s = |k: &str| v[k].as_str().map(str::to_string);
                profiles.insert(server.to_string(), (s("profile"), s("profile_hash")));
            }
            "secret_grant" | "secret_revoke" => {
                let secret = v["secret"].as_str().unwrap_or_default().to_string();
                let set = granted.entry(server.to_string()).or_default();
                if event == "secret_grant" {
                    set.insert(secret);
                    tracked.insert(server.to_string());
                } else {
                    set.remove(&secret);
                }
            }
            _ => {}
        }
    }
    let mut out = LedgerCheck::default();
    for (name, e) in lock {
        if e.revoked {
            continue;
        }
        let blocked = |why: &str| (name.clone(), why.to_string());
        let held = |s: &String| granted.get(name).is_some_and(|g| g.contains(s));
        // The ledger is the authority for the bundle record and the grants,
        // so both are compared in BOTH directions: a lock that drops the
        // `bundle` field (and with it the manifest pin), adds one the ledger
        // never approved, or (for a bundle approval) holds a secret the ledger
        // does not show as granted is not trusted, whatever else it says.
        let approved_bundle = |b: &Option<String>| match (&e.bundle, b) {
            (Some(r), Some(h)) => &r.hash == h,
            (None, None) => true,
            _ => false,
        };
        // The profile is ledger-authoritative in both directions too: the
        // lock's name and hash must be exactly what the last approval
        // recorded, and a hash the ledger never recorded is not trusted.
        // An approval with no profile line at all is the migration case.
        let profile_edited = match profiles.get(name) {
            Some((n, h)) => *n != e.sandbox || *h != e.sandbox_hash,
            None => e.sandbox_hash.is_some(),
        };
        match last.get(name) {
            Some((ev, _, _)) if ev == "revoke" => out.blocked.push(blocked(
                "the ledger's last decision for it is a revoke, but the lock says approved \
                 (edited by hand?); approve it again to use it",
            )),
            Some((_, h, _)) if *h != e.manifest_hash => out.blocked.push(blocked(
                "its manifest hash is not the one last recorded as approved in the ledger \
                 (edited by hand?); approve it again to use it",
            )),
            Some((_, _, b)) if !approved_bundle(b) => out.blocked.push(blocked(
                "its plugin manifest record is not the one last recorded as approved in the \
                 ledger (added, removed or re-pinned by hand?); approve it again to use it",
            )),
            Some(_) if profile_edited => out.blocked.push(blocked(
                "its sandbox profile record is not the one last recorded as approved in the \
                 ledger (edited by hand?); approve it again to use it",
            )),
            // Grants are ledger-authoritative for a bundle approval and for a
            // plain one that recorded them (#825). An older plain approval
            // has no grant lines: its `secrets` (#679) are reported, not
            // blocked, until the owner's next approval or `revoke --secret`.
            Some((_, _, b))
                if (b.is_some() || tracked.contains(name)) && !e.secrets.iter().all(held) =>
            {
                out.blocked.push(blocked(
                    "it holds a secret grant the ledger does not show as granted, or shows as \
                     revoked (edited by hand?); approve it again to use it",
                ))
            }
            Some((_, _, b)) => {
                if !profiles.contains_key(name) && e.source.starts_with("stdio:") {
                    out.profile_unrecorded.push(name.clone());
                }
                if b.is_none() && !tracked.contains(name) && !e.secrets.is_empty() {
                    out.secrets_unrecorded.push(name.clone());
                }
            }
            // No approval line at all. A plain entry may predate the ledger
            // (reported only), but a bundle record never does: it exists only
            // through an approval that writes a line.
            None if e.bundle.is_some() => out.blocked.push(blocked(
                "it carries a plugin manifest record the ledger has no approval for \
                 (edited by hand?); approve it again to use it",
            )),
            None if !last.is_empty() => out.unrecorded.push(name.clone()),
            None => {}
        }
    }
    out
}

#[cfg(test)]
#[path = "plugin_lock_tests.rs"]
mod tests;
