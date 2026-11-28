//! The plugin bundle manifest `tole-plugin.toml` (#811, design
//! `third-party-plugins-design.md` §9): what a third-party plugin carries
//! and asks for, as data the owner reviews before anything starts.
//!
//! ```toml
//! name = "acme"
//! version = "1.2.0"
//! description = "Acme notes"                    # optional
//! paths = ["~/notes"]                           # optional: recorded, not enforced yet
//!
//! [mcp_servers.acme-notes]                      # one table per server
//! run = "acme-mcp --stdio"                      # a command line, or:
//! # url = "https://mcp.acme.example/v1"         # a remote server (no secrets)
//! secrets = ["ACME_TOKEN"]                      # asked for, granted one by one
//!
//! [[skills]]                                    # declared, not loaded yet
//! name = "acme-triage"
//! path = "skills/triage/SKILL.md"
//!
//! [[hooks]]                                     # declared, never run
//! event = "pretool"
//! command = "./hooks/check.sh"
//! ```
//!
//! Everything in it is untrusted text: strict parsing (unknown keys are an
//! error), size limits, no control characters. Parsing activates nothing:
//! servers start only through `tole plugin add` and the lock; skills and
//! hooks are shown and hashed but never loaded or run in this slice (hooks
//! stay owner-only, skills come through the registry, a later slice); the
//! paths are recorded for the sandbox slice. The lock pins the SHA-256 of
//! the file's exact bytes ([`load`]), so any change, a comment included,
//! needs a new approval.

use std::collections::BTreeMap;
use std::io::Read;
use std::path::{Path, PathBuf};

use serde::Deserialize;
use tole_core::plugin_lock;

/// The manifest's file name.
pub const FILE: &str = "tole-plugin.toml";
/// Largest manifest read.
pub const MAX_BYTES: u64 = 64 * 1024;
const MAX_ITEMS: usize = 32;
const MAX_TEXT: usize = 500;

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Bundle {
    pub name: String,
    pub version: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub paths: Vec<String>,
    #[serde(default)]
    pub mcp_servers: BTreeMap<String, ServerDecl>,
    #[serde(default)]
    pub skills: Vec<SkillDecl>,
    #[serde(default)]
    pub hooks: Vec<HookDecl>,
}

/// One MCP server the bundle carries.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServerDecl {
    /// A command line (stdio).
    pub run: Option<String>,
    /// An https endpoint.
    pub url: Option<String>,
    /// Names of the environment secrets it asks for (stdio only).
    #[serde(default)]
    pub secrets: Vec<String>,
}

impl ServerDecl {
    /// The `--mcp-server` spec of this server under `name`.
    pub fn spec(&self, name: &str) -> String {
        format!(
            "{name}={}",
            self.run.as_deref().or(self.url.as_deref()).unwrap_or("")
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SkillDecl {
    pub name: String,
    pub path: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HookDecl {
    pub event: String,
    pub command: String,
}

/// A loaded manifest with the hash that gets locked.
#[derive(Debug, Clone)]
pub struct Loaded {
    pub bundle: Bundle,
    /// `sha256:<hex>` of the file's bytes.
    pub hash: String,
    /// Absolute path of the file.
    pub path: PathBuf,
}

fn plain(what: &str, s: &str, max: usize) -> Result<(), String> {
    if s.chars().any(char::is_control) || s.chars().count() > max {
        return Err(format!(
            "{what} has a control character or is longer than {max} characters"
        ));
    }
    Ok(())
}

fn ident(what: &str, s: &str) -> Result<(), String> {
    let ok = !s.is_empty()
        && s.len() <= 64
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_');
    ok.then_some(()).ok_or_else(|| {
        format!("{what} `{s}` must be 1-64 characters of letters, digits, `-` or `_`")
    })
}

fn few<T>(what: &str, v: &[T]) -> Result<(), String> {
    (v.len() <= MAX_ITEMS)
        .then_some(())
        .ok_or_else(|| format!("more than {MAX_ITEMS} {what}"))
}

impl Bundle {
    /// Parses and validates `text`.
    pub fn parse(text: &str) -> Result<Self, String> {
        let b: Bundle = toml::from_str(text).map_err(|e| format!("{FILE}: {e}"))?;
        b.validate().map_err(|e| format!("{FILE}: {e}"))?;
        Ok(b)
    }

    fn validate(&self) -> Result<(), String> {
        ident("plugin name", &self.name)?;
        plain("version", &self.version, 64)?;
        plain("description", &self.description, MAX_TEXT)?;
        if self.mcp_servers.is_empty() {
            return Err("it carries no MCP server".into());
        }
        few("MCP servers", &self.mcp_servers.keys().collect::<Vec<_>>())?;
        few("paths", &self.paths)?;
        few("skills", &self.skills)?;
        few("hooks", &self.hooks)?;
        for p in &self.paths {
            plain("a path", p, MAX_TEXT)?;
        }
        for (name, s) in &self.mcp_servers {
            ident("server name", name)?;
            let spec = match (&s.run, &s.url) {
                (Some(r), None) => r,
                (None, Some(u)) => u,
                _ => return Err(format!("server `{name}` needs exactly one of run and url")),
            };
            plain("a server command or url", spec, 2048)?;
            few("secrets", &s.secrets)?;
            if s.url.is_some() && !s.secrets.is_empty() {
                return Err(format!(
                    "server `{name}` is remote and asks for secrets: a grant reaches only a \
                     process tole starts"
                ));
            }
            for (i, n) in s.secrets.iter().enumerate() {
                let shaped = !n.is_empty()
                    && n.len() <= 64
                    && n.starts_with(|c: char| c.is_ascii_uppercase())
                    && n.bytes()
                        .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit() || b == b'_');
                if !shaped || !tole_core::secrets::is_secret_name(n) {
                    return Err(format!(
                        "server `{name}`: `{n}` is not a secret name (upper case, with a marker \
                         such as TOKEN, KEY or SECRET)"
                    ));
                }
                if s.secrets[..i].contains(n) {
                    return Err(format!("server `{name}` lists `{n}` twice"));
                }
            }
        }
        for k in &self.skills {
            plain("a skill name", &k.name, MAX_TEXT)?;
            plain("a skill path", &k.path, MAX_TEXT)?;
        }
        for h in &self.hooks {
            plain("a hook event", &h.event, MAX_TEXT)?;
            plain("a hook command", &h.command, MAX_TEXT)?;
        }
        Ok(())
    }
}

/// Reads the manifest at `path` (a file, or a directory holding
/// [`FILE`]) once: the bytes that are hashed are the bytes that are parsed.
pub fn load(path: &Path) -> Result<Loaded, String> {
    let path = if path.is_dir() {
        path.join(FILE)
    } else {
        path.to_path_buf()
    };
    let path = path
        .canonicalize()
        .map_err(|e| format!("{}: {e}", path.display()))?;
    let bytes = read(&path)?;
    let text =
        std::str::from_utf8(&bytes).map_err(|_| format!("{}: not UTF-8 text", path.display()))?;
    Ok(Loaded {
        bundle: Bundle::parse(text)?,
        hash: hash_bytes(&bytes),
        path,
    })
}

fn read(path: &Path) -> Result<Vec<u8>, String> {
    let f = std::fs::File::open(path).map_err(|e| format!("{}: {e}", path.display()))?;
    if !f.metadata().map_err(|e| e.to_string())?.is_file() {
        return Err(format!("{}: not a regular file", path.display()));
    }
    let mut buf = Vec::new();
    f.take(MAX_BYTES + 1)
        .read_to_end(&mut buf)
        .map_err(|e| format!("{}: {e}", path.display()))?;
    if buf.len() as u64 > MAX_BYTES {
        return Err(format!("{}: larger than {MAX_BYTES} bytes", path.display()));
    }
    Ok(buf)
}

fn hash_bytes(bytes: &[u8]) -> String {
    plugin_lock::sha(bytes)
}

/// The hash of the manifest file at `path` now; empty when it cannot be
/// read (the lock then refuses the server).
pub fn file_hash(path: &str) -> String {
    read(Path::new(path))
        .map(|b| hash_bytes(&b))
        .unwrap_or_default()
}

/// The hash of `entry`'s manifest file now, or `None` for an entry without a
/// bundle record.
pub fn live_hash(entry: &plugin_lock::LockEntry) -> Option<String> {
    entry.bundle.as_ref().map(|b| file_hash(&b.path))
}
