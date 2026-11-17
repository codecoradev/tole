//! Per-session profiles for `tole serve` (#740).
//!
//! `POST /sessions {"profile": "<name>"}` (create and resume) selects one
//! `[profiles.<name>]` table of the USER config for that session. The rules,
//! all enforced here so the route stays thin:
//!
//! - **Source.** Only the user config is read (a project config cannot define
//!   profiles, see `config::layered`), re-read on every selection so an edit
//!   or a removal applies to the next create/resume without a restart. A name
//!   that is not defined is an error, never a silent default.
//! - **Fixed.** The choice is made when the session process opens the session
//!   and never changes while it lives; the caller records it in the session
//!   log (a `fact/profile` register: a new entry, never a mutation).
//! - **Allow list.** The same precedence `config/apply.rs` gives `--profile`:
//!   an `--allow` flag of the serve command line pins the daemon's list and
//!   the profile's is ignored; otherwise a profile that sets `allow` replaces
//!   the daemon's (config or env) list, and a profile without one leaves it.
//!   `--trust` presets are added in every case. Every approval still goes
//!   through the session's `Approver`.
//! - **Memory room.** The profile's `memory_room` (default
//!   `profile-<name>`), sanitized like the CLI's; the request can never set
//!   it. A child's `TOLE_MEMORY_NAMESPACE` still wins, as on the CLI.
//! - **Model.** The profile's `default` tier model when it has one, else the
//!   server default.

use crate::config::{self, profile::Profile};
use std::path::{Path, PathBuf};
use tole_core::memory::MemoryConfig;

/// What `tole serve` knows at start-up about where profiles come from and
/// what its own command line permits.
#[derive(Debug, Clone, Default)]
pub struct ProfileSource {
    /// The user config file (`None` under `--no-config`: no profiles).
    pub config: Option<PathBuf>,
    /// The daemon's own allow list (`--allow` > profile of `--profile` >
    /// env > config), without the trust presets.
    pub allow: Vec<String>,
    /// The expanded `--trust` presets, appended in every case.
    pub trust: Vec<String>,
    /// True when `--allow` was given on the command line: the profile's
    /// allow list is then ignored (the flag is the highest layer).
    pub allow_pinned: bool,
}

/// A profile as one session uses it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionProfile {
    pub name: String,
    /// The persona text, when the profile names a persona file.
    pub system_prompt: Option<String>,
    /// The profile's model, when it sets one.
    pub model: Option<String>,
    /// The memory room (already a valid room name).
    pub memory_room: String,
    /// The session's effective allow patterns (trust presets included).
    pub allow: Vec<String>,
}

/// Why a selection failed. `status()` is the HTTP answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProfileError {
    /// Not a profile name at all (shape), or not defined.
    Rejected(String),
    /// The profile exists but cannot be used (unreadable persona, broken
    /// user config): a server-side problem.
    Unusable(String),
}

impl ProfileError {
    pub fn status(&self) -> u16 {
        match self {
            ProfileError::Rejected(_) => 400,
            ProfileError::Unusable(_) => 500,
        }
    }

    pub fn message(&self) -> &str {
        match self {
            ProfileError::Rejected(m) | ProfileError::Unusable(m) => m,
        }
    }
}

impl ProfileSource {
    /// Select profile `name`. The name is checked for shape BEFORE it is
    /// looked up or echoed, so a hostile value never reaches a message, a
    /// path or a log line.
    pub fn select(&self, name: &str) -> Result<SessionProfile, ProfileError> {
        if !config::valid_profile_name(name) {
            return Err(ProfileError::Rejected(
                "invalid profile name: 1-64 chars of a-z, 0-9, `_` or `-`, not starting with `-`"
                    .into(),
            ));
        }
        let Some(path) = self.config.as_deref() else {
            return Err(unknown_none(name));
        };
        let cfg = match config::user_config_present(path) {
            Ok(true) => config::load(path)
                .map_err(|e| ProfileError::Unusable(format!("the user config is invalid: {e}")))?,
            Ok(false) => return Err(unknown_none(name)),
            Err(e) => return Err(ProfileError::Unusable(e)),
        };
        let base = path.parent().map(Path::to_path_buf);
        let home = std::env::var_os("HOME").map(PathBuf::from);
        let p = Profile::resolve(&cfg, name, base.as_deref(), home.as_deref()).map_err(|e| {
            // `resolve` words "not defined" and "cannot load" the same way;
            // only the first is the client's mistake.
            if e.starts_with("unknown profile") {
                ProfileError::Rejected(e)
            } else {
                ProfileError::Unusable(format!("profile `{name}` cannot be loaded: {e}"))
            }
        })?;
        Ok(self.session_profile(&p))
    }

    fn session_profile(&self, p: &Profile) -> SessionProfile {
        SessionProfile {
            name: p.name.clone(),
            system_prompt: p.system_prompt.clone().filter(|s| !s.trim().is_empty()),
            model: p.model.clone().filter(|m| !m.trim().is_empty()),
            memory_room: tole_core::memory::sanitize_namespace(&p.memory_room),
            allow: self.effective_allow(&p.allow),
        }
    }

    /// The allow precedence of the module doc.
    pub fn effective_allow(&self, profile_allow: &[String]) -> Vec<String> {
        let mut out = if self.allow_pinned || profile_allow.is_empty() {
            self.allow.clone()
        } else {
            profile_allow.to_vec()
        };
        out.extend(self.trust.iter().cloned());
        out
    }

    /// The allow list of a session opened without a profile: unchanged.
    pub fn default_allow(&self) -> Vec<String> {
        let mut out = self.allow.clone();
        out.extend(self.trust.iter().cloned());
        out
    }
}

fn unknown_none(name: &str) -> ProfileError {
    ProfileError::Rejected(format!(
        "unknown profile `{name}`: the user config defines no profiles"
    ))
}

impl SessionProfile {
    /// The session's memory config: the server's, moved to the profile's
    /// room. `None` stays `None` (a profile cannot switch the memory loop
    /// on), and a child's mailbox (`env_room`) still wins.
    pub fn memory(
        &self,
        base: Option<&MemoryConfig>,
        env_room: Option<&str>,
    ) -> Option<MemoryConfig> {
        let mut m = base?.clone();
        if env_room.is_none_or(|r| r.trim().is_empty()) {
            m.room = self.memory_room.clone();
        }
        Some(m)
    }
}

/// The profile a session log recorded (`fact/profile`), if its name is
/// still a well-formed profile name (a tampered log never yields a path or
/// message fragment).
pub fn recorded(storage: &dyn tole_core::storage::Storage) -> Option<String> {
    let v = storage.get_register(REGISTER_NS, REGISTER_KEY)?;
    let name = v.get("name")?.as_str()?;
    config::valid_profile_name(name).then(|| name.to_string())
}

pub const REGISTER_NS: &str = "fact";
pub const REGISTER_KEY: &str = "profile";

/// The register write that records `name` as this session's profile.
pub fn record_write(name: &str) -> tole_core::register::RegisterWrite {
    tole_core::register::RegisterWrite::set(
        REGISTER_NS,
        REGISTER_KEY,
        serde_json::json!({
            "name": name,
            "atMs": std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis() as u64)
                .unwrap_or(0),
        }),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn src(allow: &[&str], trust: &[&str], pinned: bool) -> ProfileSource {
        ProfileSource {
            config: None,
            allow: allow.iter().map(|s| s.to_string()).collect(),
            trust: trust.iter().map(|s| s.to_string()).collect(),
            allow_pinned: pinned,
        }
    }

    fn v(a: &[&str]) -> Vec<String> {
        a.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn a_profile_allow_replaces_the_daemon_list_unless_the_flag_pins_it() {
        let open = src(&["env_tool"], &["todo_write"], false);
        assert_eq!(
            open.effective_allow(&v(&["write_file"])),
            v(&["write_file", "todo_write"])
        );
        // A profile without an allow list leaves the daemon's.
        assert_eq!(open.effective_allow(&[]), v(&["env_tool", "todo_write"]));
        // `--allow` pins: the profile can never widen it.
        let pinned = src(&["read_only_ok"], &[], true);
        assert_eq!(
            pinned.effective_allow(&v(&["write_file", "run_command"])),
            v(&["read_only_ok"])
        );
        assert_eq!(open.default_allow(), v(&["env_tool", "todo_write"]));
    }

    #[test]
    fn a_hostile_name_is_refused_before_any_lookup() {
        let s = src(&[], &[], false);
        for bad in [
            "",
            "-x",
            "A",
            "a b",
            "../x",
            "x\ny",
            &"a".repeat(65),
            "a\u{1b}[31m",
        ] {
            let e = s.select(bad).unwrap_err();
            assert_eq!(e.status(), 400, "{bad:?}");
            assert!(
                !e.message().contains(bad) || bad.is_empty(),
                "echoed {bad:?}"
            );
        }
    }

    #[test]
    fn no_config_means_no_profiles() {
        let e = src(&[], &[], false).select("kai").unwrap_err();
        assert_eq!(e.status(), 400);
        assert!(e.message().contains("no profiles"));
    }

    #[test]
    fn the_memory_room_follows_the_profile_but_never_enables_memory() {
        let p = SessionProfile {
            name: "kai".into(),
            system_prompt: None,
            model: None,
            memory_room: "profile-kai".into(),
            allow: vec![],
        };
        assert!(p.memory(None, None).is_none());
        let base = MemoryConfig {
            bin: "uteke".into(),
            room: "repo-x".into(),
            limit: 3,
        };
        assert_eq!(p.memory(Some(&base), None).unwrap().room, "profile-kai");
        assert_eq!(
            p.memory(Some(&base), Some(" ")).unwrap().room,
            "profile-kai"
        );
        assert_eq!(
            p.memory(Some(&base), Some("mailbox")).unwrap().room,
            "repo-x"
        );
    }
}
