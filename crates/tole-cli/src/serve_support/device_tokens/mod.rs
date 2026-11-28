//! Per-device named tokens for `tole serve` (#1320): the store, the `tole serve token` commands
//! and, through [`Live`], the REST face's check (`Authenticator::authenticate`; the MCP face
//! does not take device tokens).
//!
//! A device token is a 256-bit secret from the OS random source, shown once
//! by `tole serve token add` and never stored: the file keeps only its
//! SHA-256 (a fast hash is right for a random 256-bit secret, and a request
//! path cannot afford PBKDF2), a name, a scope and times.
//!
//! The file is `<tole data dir>/serve-tokens.jsonl`, owner-only (0600, a data
//! dir that is not group or world writable) and append-only **by convention**
//! (nothing in tole rewrites it; the file system does not enforce it):
//! `add`, `revoke` and `used` are lines, and a revoke is a new line, never a
//! deletion. The state is the fold of the lines. A line that does not read
//! back, a name added twice, a hash that was added before (a revoked one
//! included), a revoke of a name that is not active, an unknown record
//! version or a file over its cap makes the whole store unusable
//! ([`StoreError::Corrupt`]): a skipped revoke line would bring a revoked
//! token back, so nothing is guessed.
//!
//! What the file rules do and do not cover. The last component of the token
//! file and of the lock file is opened with `O_NOFOLLOW` (unix only); the
//! parent directories are followed, as the other tole stores do. The owner of
//! the file is not compared with the effective uid (that needs `unsafe`), so
//! the protection is the mode of the file and of the data dir, not who owns
//! them. **Restoring the file from a backup brings back every token revoked
//! since the backup was taken: rotate all tokens after a restore.**
//!
//! The whole surface `tole serve` uses is [`Live`]:
//! `live.lookup(presented)` is `Option<Principal>` (name and scope) and
//! `live.touch(&name, now)` records the last use at most once per
//! [`TOUCH_EVERY_SECS`] per name. A request costs one `stat` of the file
//! (an append always changes its length, so a revoke is seen on the next
//! request) and a comparison over every active hash with no early exit.
//!
//! Mind the cost model: `Live::lookup` holds a mutex while it
//! reloads the file after a change, and `Live::touch` does synchronous file
//! I/O under a try-lock. Call both from a thread that may block (the REST
//! face's connection thread), or from `spawn_blocking`, not from an async task. `used` lines are never compacted: past
//! [`MAX_USED_FILE_BYTES`] last-used stops being recorded (the file is not
//! rewritten to make room). A load that fails with an I/O error is retried
//! after [`RETRY_AFTER_SECS`] or when the file changes; a corrupt, unsafe or
//! oversized file stays refused until the file changes.

mod cmd;
mod store;
#[cfg(test)]
mod tests;

pub use cmd::{cli, run, ServeAction, TokenAction};
pub use store::{Added, Live, Store};

use serde::{Deserialize, Serialize};

use super::auth::ct_eq;

/// Every token starts with this, so a secret scanner can find a leaked one
/// and `lookup` can refuse a static bearer token without any work.
pub const PREFIX: &str = "tole_dt_";
/// Random bytes of a token (256 bits).
const TOKEN_BYTES: usize = 32;
/// Length of a token: the prefix and 32 bytes in unpadded base64.
const TOKEN_LEN: usize = PREFIX.len() + 43;
/// Longest name.
pub const MAX_NAME: usize = 64;
/// Most active tokens (the comparison visits each one on every request).
pub const MAX_ACTIVE: usize = 64;
/// Longest record line; a longer one is corrupt.
const MAX_LINE: usize = 1024;
/// A file over this is refused (fail closed), never read.
pub const MAX_FILE_BYTES: u64 = 4 * 1024 * 1024;
/// `used` lines stop at this size, so `add` and `revoke` keep room.
pub const MAX_USED_FILE_BYTES: u64 = 3 * 1024 * 1024;
/// Last use is written at most this often per token.
pub const TOUCH_EVERY_SECS: u64 = 15 * 60;
/// `Live` retries a load that failed with an I/O error after this long.
pub const RETRY_AFTER_SECS: u64 = 5;
const VERSION: u8 = 1;
const HASH_PREFIX: &str = "sha256:";

/// What a token may do: `tole serve` enforces it on every route.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, clap::ValueEnum)]
pub enum Scope {
    /// Reads sessions and the event stream; answers no approval.
    #[serde(rename = "read-only")]
    #[value(name = "read-only")]
    ReadOnly,
    /// Everything the shared bearer token can do.
    #[serde(rename = "full")]
    #[value(name = "full")]
    Full,
}

impl Scope {
    pub fn as_str(self) -> &'static str {
        match self {
            Scope::ReadOnly => "read-only",
            Scope::Full => "full",
        }
    }
}

/// Who a presented token belongs to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Principal {
    pub name: String,
    pub scope: Scope,
}

/// A token value. `Debug` hides it and there is no `Display`: the only way
/// out is [`Secret::expose`], called once, to print it.
pub struct Secret(String);

impl Secret {
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Debug for Secret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Secret(<redacted>)")
    }
}

/// Why the store refused. No variant carries a token, a hash or the text of a
/// record line.
#[derive(Debug)]
pub enum StoreError {
    BadName(&'static str),
    /// An active token has this name.
    Duplicate(String),
    /// No active token has this name.
    NotFound(String),
    TooMany,
    /// The file is not usable; `line` is 1-based.
    Corrupt {
        line: usize,
        why: &'static str,
    },
    /// Wrong file type or permissions.
    Unsafe(String),
    TooLarge,
    NoDataDir,
    Random,
    Io(std::io::Error),
}

impl std::fmt::Display for StoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            StoreError::BadName(why) => write!(f, "invalid token name: {why}"),
            StoreError::Duplicate(n) => write!(f, "an active token named {n:?} exists"),
            StoreError::NotFound(n) => write!(f, "no active token named {n:?}"),
            StoreError::TooMany => {
                write!(f, "at most {MAX_ACTIVE} active tokens; revoke one first")
            }
            StoreError::Corrupt { line, why } => write!(
                f,
                "the token file is corrupt at line {line} ({why}); no device token is valid until \
                 it is repaired by hand"
            ),
            StoreError::Unsafe(why) => write!(f, "the token file is not safe to use: {why}"),
            StoreError::TooLarge => write!(f, "the token file is over {MAX_FILE_BYTES} bytes"),
            StoreError::NoDataDir => {
                write!(
                    f,
                    "cannot resolve the tole data dir (set CODECORA_HOME or HOME)"
                )
            }
            StoreError::Random => write!(f, "the system random source failed"),
            StoreError::Io(e) => write!(f, "token file: {e}"),
        }
    }
}

impl std::error::Error for StoreError {}

impl StoreError {
    /// Is the failure a property of the file (it stays until the file
    /// changes) and not of the moment (an I/O error may pass)?
    pub fn is_deterministic(&self) -> bool {
        !matches!(self, StoreError::Io(_) | StoreError::Random)
    }
}

impl From<std::io::Error> for StoreError {
    fn from(e: std::io::Error) -> Self {
        StoreError::Io(e)
    }
}

/// `[a-z0-9][a-z0-9._-]{0,63}`: lower case only, so two names never differ
/// by case, and nothing a terminal or a log could misread.
pub fn validate_name(name: &str) -> Result<(), &'static str> {
    let b = name.as_bytes();
    if b.is_empty() || b.len() > MAX_NAME {
        return Err("1 to 64 characters");
    }
    let first = b[0].is_ascii_lowercase() || b[0].is_ascii_digit();
    let rest = b
        .iter()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, b'.' | b'_' | b'-'));
    if first && rest {
        Ok(())
    } else {
        Err("lower-case letters, digits, '.', '_' and '-', starting with a letter or digit")
    }
}

/// Is this the shape of a device token? Public knowledge, so a refusal here
/// leaks nothing.
pub(crate) fn shaped(presented: &str) -> bool {
    presented.len() == TOKEN_LEN && presented.starts_with(PREFIX)
}

fn sha256(token: &str) -> [u8; 32] {
    let d = ring::digest::digest(&ring::digest::SHA256, token.as_bytes());
    let mut out = [0u8; 32];
    out.copy_from_slice(d.as_ref());
    out
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn unhex(s: &str) -> Option<[u8; 32]> {
    let s = s.strip_prefix(HASH_PREFIX)?;
    if s.len() != 64
        || !s
            .bytes()
            .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c))
    {
        return None;
    }
    let mut out = [0u8; 32];
    for (i, o) in out.iter_mut().enumerate() {
        *o = u8::from_str_radix(&s[2 * i..2 * i + 2], 16).ok()?;
    }
    Some(out)
}

/// A new token from the OS random source, and the hash that is stored.
fn generate() -> Result<(Secret, [u8; 32]), StoreError> {
    use base64::Engine;
    use ring::rand::SecureRandom;
    let mut raw = [0u8; TOKEN_BYTES];
    ring::rand::SystemRandom::new()
        .fill(&mut raw)
        .map_err(|_| StoreError::Random)?;
    let token = format!(
        "{PREFIX}{}",
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(raw)
    );
    let hash = sha256(&token);
    Ok((Secret(token), hash))
}

/// One line of the file.
#[derive(Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
enum Line {
    Add {
        v: u8,
        name: String,
        scope: Scope,
        hash: String,
        at: u64,
    },
    Revoke {
        v: u8,
        name: String,
        at: u64,
    },
    Used {
        v: u8,
        name: String,
        at: u64,
    },
}

impl Line {
    fn add(name: &str, scope: Scope, hash: &[u8; 32], at: u64) -> Line {
        Line::Add {
            v: VERSION,
            name: name.to_string(),
            scope,
            hash: format!("{HASH_PREFIX}{}", hex(hash)),
            at,
        }
    }
    fn revoke(name: &str, at: u64) -> Line {
        Line::Revoke {
            v: VERSION,
            name: name.to_string(),
            at,
        }
    }
    fn used(name: &str, at: u64) -> Line {
        Line::Used {
            v: VERSION,
            name: name.to_string(),
            at,
        }
    }
    fn version(&self) -> u8 {
        match self {
            Line::Add { v, .. } | Line::Revoke { v, .. } | Line::Used { v, .. } => *v,
        }
    }
    /// The line and its newline, for one `write`.
    fn encode(&self) -> Result<Vec<u8>, StoreError> {
        let mut v = serde_json::to_vec(self).map_err(|e| StoreError::Io(e.into()))?;
        v.push(b'\n');
        Ok(v)
    }
}

/// A token as the file records it (never the hash, in `Debug` too).
#[derive(Clone)]
pub struct Entry {
    pub name: String,
    pub scope: Scope,
    pub created_at: u64,
    pub last_used_at: Option<u64>,
    pub revoked_at: Option<u64>,
    hash: [u8; 32],
}

impl Entry {
    pub fn active(&self) -> bool {
        self.revoked_at.is_none()
    }
}

impl std::fmt::Debug for Entry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Entry")
            .field("name", &self.name)
            .field("scope", &self.scope)
            .field("created_at", &self.created_at)
            .field("last_used_at", &self.last_used_at)
            .field("revoked_at", &self.revoked_at)
            .finish_non_exhaustive()
    }
}

/// The fold of the file: every token ever added, oldest first.
#[derive(Debug, Clone, Default)]
pub struct Snapshot {
    entries: Vec<Entry>,
}

impl Snapshot {
    pub fn entries(&self) -> &[Entry] {
        &self.entries
    }

    pub fn active(&self, name: &str) -> Option<&Entry> {
        self.entries.iter().find(|e| e.active() && e.name == name)
    }

    fn active_count(&self) -> usize {
        self.entries.iter().filter(|e| e.active()).count()
    }

    /// Reads the file's bytes. Any line that does not read back is an error,
    /// and so is a revoke of a name that is not active. A use of a name that
    /// is not active (or was revoked) is ignored: it changes nothing but the
    /// last-used time of an active token.
    pub fn parse(bytes: &[u8]) -> Result<Snapshot, StoreError> {
        if bytes.len() as u64 > MAX_FILE_BYTES {
            return Err(StoreError::TooLarge);
        }
        let mut snap = Snapshot::default();
        for (i, raw) in bytes.split(|b| *b == b'\n').enumerate() {
            let line = i + 1;
            let bad = |why| StoreError::Corrupt { line, why };
            if raw.iter().all(u8::is_ascii_whitespace) {
                continue;
            }
            if raw.len() > MAX_LINE {
                return Err(bad("line too long"));
            }
            // The parser's own message is dropped: it may quote the line.
            let rec: Line = serde_json::from_slice(raw).map_err(|_| bad("not a record"))?;
            if rec.version() != VERSION {
                return Err(bad("unsupported record version"));
            }
            match rec {
                Line::Add {
                    name,
                    scope,
                    hash,
                    at,
                    ..
                } => {
                    validate_name(&name).map_err(|_| bad("invalid name"))?;
                    let hash = unhex(&hash).ok_or_else(|| bad("invalid hash"))?;
                    if snap.active(&name).is_some() {
                        return Err(bad("name added twice without a revoke"));
                    }
                    // Revoked entries count: a hash that was ever added is not
                    // added again, so a revoked token cannot be re-added.
                    if snap.entries.iter().any(|e| e.hash == hash) {
                        return Err(bad("hash added twice"));
                    }
                    if snap.active_count() >= MAX_ACTIVE {
                        return Err(bad("too many active tokens"));
                    }
                    snap.entries.push(Entry {
                        name,
                        scope,
                        created_at: at,
                        last_used_at: None,
                        revoked_at: None,
                        hash,
                    });
                }
                Line::Revoke { name, at, .. } => {
                    let e = snap
                        .entries
                        .iter_mut()
                        .find(|e| e.active() && e.name == name)
                        .ok_or_else(|| bad("revoke of a name that is not active"))?;
                    e.revoked_at = Some(at);
                }
                Line::Used { name, at, .. } => {
                    if let Some(e) = snap
                        .entries
                        .iter_mut()
                        .find(|e| e.active() && e.name == name)
                    {
                        e.last_used_at = Some(e.last_used_at.map_or(at, |t| t.max(at)));
                    }
                }
            }
        }
        Ok(snap)
    }

    /// Whose token is this? `None` for anything that is not an active device
    /// token. A token of the wrong shape is refused before any work (the
    /// shape is not secret); otherwise its hash is compared with every active
    /// hash, in constant time and without stopping at the first match.
    pub fn lookup(&self, presented: &str) -> Option<Principal> {
        if !shaped(presented) {
            return None;
        }
        let h = sha256(presented);
        let mut found: Option<&Entry> = None;
        for e in self.entries.iter().filter(|e| e.active()) {
            if ct_eq(&e.hash, &h) {
                found = Some(e);
            }
        }
        found.map(|e| Principal {
            name: e.name.clone(),
            scope: e.scope,
        })
    }
}
