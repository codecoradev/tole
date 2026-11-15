//! Named sandbox profiles and per-server bindings (#810, design
//! `docs/sandbox-design.md` section 6).
//!
//! ```toml
//! [sandbox.profiles.net]          # a name of your own (not `server` or `strict`)
//! workspace = "read"              # none | read | write   (default read)
//! network = "host"                # none | host           (default none)
//! read = ["/opt/data"]
//!
//! [sandbox.servers.github]        # the MCP server NAME, not `name@workspace`
//! profile = "net"
//! ```
//!
//! USER LAYER ONLY, with the rest of `[sandbox]`: the layering copies the
//! user's table and nothing of a project's, so a repository can neither
//! define a profile nor bind a server to a wider one. `tole sandbox` writes
//! the user config only.

use serde::Deserialize;
use std::collections::BTreeMap;
use tole_core::sandbox::{builtin_profile, Network, Profile, ServerProfile, Workspace};

use super::SandboxConfig;

/// One `[sandbox.profiles.<name>]` table.
#[derive(Debug, Default, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SandboxProfileConfig {
    /// `none` | `read` (default) | `write`.
    pub workspace: Option<String>,
    /// `none` (default) or `host`.
    pub network: Option<String>,
    /// Extra read-only paths.
    pub read: Option<Vec<String>>,
    /// Extra writable paths.
    pub write: Option<Vec<String>>,
}

/// One `[sandbox.servers.<name>]` table.
#[derive(Debug, Default, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SandboxServerConfig {
    /// A built-in (`server`, `strict`) or defined profile.
    pub profile: Option<String>,
}

/// The built-in profile names, which a table cannot redefine.
pub const BUILTIN_PROFILES: [&str; 2] = ["server", "strict"];

/// Where an effective profile comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    /// `[sandbox.servers.<name>]` in the user config.
    Server,
    /// `[sandbox] profile` in the user config.
    UserDefault,
    /// Nothing set: the built-in `server` profile.
    BuiltinDefault,
}

impl Source {
    pub fn describe(self) -> &'static str {
        match self {
            Source::Server => "user config [sandbox.servers]",
            Source::UserDefault => "user config [sandbox] profile",
            Source::BuiltinDefault => "default",
        }
    }
}

/// The profile one server runs in, and why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Effective {
    pub name: String,
    pub profile: Profile,
    pub source: Source,
}

fn name_ok(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 64
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

fn paths(v: &Option<Vec<String>>) -> Vec<std::path::PathBuf> {
    v.iter().flatten().map(Into::into).collect()
}

fn network(key: &str, v: Option<&str>) -> Result<Option<Network>, String> {
    match v {
        None => Ok(None),
        Some("none") => Ok(Some(Network::None)),
        Some("host") => Ok(Some(Network::Host)),
        Some(s) => Err(format!("{key} must be none or host, got {s:?}")),
    }
}

impl SandboxProfileConfig {
    fn build(&self, name: &str) -> Result<Profile, String> {
        let key = format!("sandbox.profiles.{name}");
        let workspace = match self.workspace.as_deref() {
            None | Some("read") => Workspace::Read,
            Some("none") => Workspace::None,
            Some("write") => Workspace::Write,
            Some(s) => {
                return Err(format!(
                    "{key}.workspace must be none, read or write, got {s:?}"
                ))
            }
        };
        Ok(Profile {
            workspace,
            network: network(&format!("{key}.network"), self.network.as_deref())?
                .unwrap_or(Network::None),
            read: paths(&self.read),
            write: paths(&self.write),
            protect: Vec::new(),
        })
    }
}

impl SandboxConfig {
    /// The profile called `name`: a built-in one or one this table defines.
    pub fn resolve_profile(&self, name: &str) -> Result<Profile, String> {
        if let Some(p) = builtin_profile(name) {
            return Ok(p);
        }
        match self.profiles.as_ref().and_then(|m| m.get(name)) {
            Some(def) => def.build(name),
            None => {
                let defined: Vec<&str> = self
                    .profiles
                    .iter()
                    .flatten()
                    .map(|(k, _)| k.as_str())
                    .collect();
                Err(format!(
                    "unknown sandbox profile {name:?} (built in: server, strict; defined: {})",
                    if defined.is_empty() {
                        "none".to_string()
                    } else {
                        defined.join(", ")
                    }
                ))
            }
        }
    }

    /// Checks every defined profile and server binding; a bad entry is an
    /// error naming its key.
    pub fn check_tables(&self) -> Result<(), String> {
        for (name, def) in self.profiles.iter().flatten() {
            if !name_ok(name) {
                return Err(format!(
                    "sandbox.profiles.{name}: a profile name is 1 to 64 of A-Z a-z 0-9 - _"
                ));
            }
            if BUILTIN_PROFILES.contains(&name.as_str()) {
                return Err(format!(
                    "sandbox.profiles.{name}: `server` and `strict` are built in and cannot be redefined; pick another name"
                ));
            }
            def.build(name)?;
        }
        self.server_profiles().map(drop)
    }

    /// The per-server bindings, each resolved to its profile.
    pub fn server_profiles(&self) -> Result<BTreeMap<String, ServerProfile>, String> {
        let mut out = BTreeMap::new();
        for (server, def) in self.servers.iter().flatten() {
            if server.is_empty() || server.contains('@') {
                return Err(format!(
                    "sandbox.servers.{server:?}: use the server name, without `@workspace`"
                ));
            }
            let name = def
                .profile
                .as_deref()
                .ok_or_else(|| format!("sandbox.servers.{server}.profile is required"))?;
            let profile = self
                .resolve_profile(name)
                .map_err(|e| format!("sandbox.servers.{server}.profile: {e}"))?;
            out.insert(
                server.clone(),
                ServerProfile {
                    name: name.to_string(),
                    profile,
                },
            );
        }
        Ok(out)
    }

    /// The profile `server` runs in and where that comes from.
    pub fn effective(&self, server: &str) -> Result<Effective, String> {
        if let Some(def) = self.servers.as_ref().and_then(|m| m.get(server)) {
            let name = def
                .profile
                .as_deref()
                .ok_or_else(|| format!("sandbox.servers.{server}.profile is required"))?;
            return Ok(Effective {
                name: name.to_string(),
                profile: self.resolve_profile(name)?,
                source: Source::Server,
            });
        }
        let policy = self.policy()?;
        Ok(Effective {
            name: policy.profile_name,
            profile: policy.profile,
            source: if self.profile.is_some() {
                Source::UserDefault
            } else {
                Source::BuiltinDefault
            },
        })
    }
}

/// `tole config check` lines for the effective profiles: the default and
/// each bound server, each with its source. Only the user layer can
/// contribute, so the source is never a project config.
pub fn effective_lines(cfg: &SandboxConfig) -> Vec<String> {
    let mut out = Vec::new();
    match cfg.effective("") {
        Ok(e) => out.push(format!(
            "sandbox effective: default profile {:?} [{}]",
            e.name,
            e.source.describe()
        )),
        Err(e) => out.push(format!("sandbox effective: invalid ({e})")),
    }
    for server in cfg.servers.iter().flatten().map(|(k, _)| k) {
        out.push(match cfg.effective(server) {
            Ok(e) => format!(
                "sandbox effective: server {server:?} profile {:?} [{}]",
                e.name,
                e.source.describe()
            ),
            Err(e) => format!("sandbox effective: server {server:?} invalid ({e})"),
        });
    }
    out
}
