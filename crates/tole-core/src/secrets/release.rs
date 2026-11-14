//! Scoped secret release (#598, design `secrets-integrations-design.md` §4,
//! §8.3): which consumers may receive each secret in the environment of the
//! processes they start.
//!
//! A consumer is a tool name (`gh`, `git`, `run_command`, `job_start`) or one
//! of tole's own process launchers (`hook`, `mcp`, `cron`, `uteke`,
//! `memory`, `cora`, `skills`, [`VERIFY`], [`TURNEND`]). Every launcher
//! scrubs the secret-shaped variables ([`crate::secrets::is_secret_name`],
//! ENV-1) that the release does not bind to it
//! ([`crate::subprocess::scrub_env_for`]).
//!
//! The default binds only `GITHUB_TOKEN`, to the `gh` and `git` tools and to
//! the owner's own hooks and MCP servers. `run_command`, `job_start` and cron
//! scripts receive NO secret unless a `[secrets]` entry grants one to them,
//! and neither do the mission verify command and the turn-end stop gates
//! (#676): both usually run code the agent just wrote (`cargo test`).
//!
//! Release decides what ENTERS a child; it never changes redaction
//! ([`crate::secrets::SecretSet`]), which covers every secret, granted or
//! not, on what LEAVES the process.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::RwLock;

/// The consumer of the `tole mission --verify` command (#676).
pub const VERIFY: &str = "verify";

/// The consumer of the `on_turnend` stop gates (#676). Separate from `hook`
/// (pre/post tool hooks), which keeps `GITHUB_TOKEN` by default.
pub const TURNEND: &str = "turnend";

/// The bindings used when no `[secrets]` entry names a secret.
pub const DEFAULT_RELEASE: [(&str, &[&str]); 1] = [("GITHUB_TOKEN", &["gh", "git", "hook", "mcp"])];

/// Secret NAME (upper case) to the consumers that may receive it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Release {
    by_name: BTreeMap<String, BTreeSet<String>>,
}

impl Default for Release {
    fn default() -> Self {
        let mut r = Release {
            by_name: BTreeMap::new(),
        };
        for (name, tools) in DEFAULT_RELEASE {
            r = r.with(name, tools.iter().map(|t| t.to_string()));
        }
        r
    }
}

impl Release {
    /// This release with `name` bound to exactly `tools` (replacing its
    /// previous binding; an empty list binds it to nothing).
    pub fn with(mut self, name: &str, tools: impl IntoIterator<Item = String>) -> Self {
        self.by_name
            .insert(name.to_ascii_uppercase(), tools.into_iter().collect());
        self
    }

    /// May `consumer` receive the secret `name` (case-insensitive)?
    pub fn allows(&self, name: &str, consumer: &str) -> bool {
        self.by_name
            .get(&name.to_ascii_uppercase())
            .is_some_and(|t| t.contains(consumer))
    }

    /// The consumers bound to `name`, when it has a binding.
    pub fn tools(&self, name: &str) -> Option<&BTreeSet<String>> {
        self.by_name.get(&name.to_ascii_uppercase())
    }

    /// Names bound to at least one consumer: what a session may pass on.
    pub fn granted_names(&self) -> impl Iterator<Item = &str> {
        self.by_name
            .iter()
            .filter(|(_, t)| !t.is_empty())
            .map(|(n, _)| n.as_str())
    }

    /// The secret-shaped variables this process holds that are granted to
    /// some consumer: the most a child session may be given (#585 then
    /// intersects with a profile's grants).
    pub fn held_grants(&self) -> Vec<String> {
        std::env::vars_os()
            .filter_map(|(k, _)| k.into_string().ok())
            .filter(|k| crate::secrets::is_secret_name(k))
            .filter(|k| self.tools(k).is_some_and(|t| !t.is_empty()))
            .collect()
    }
}

static RELEASE: RwLock<Option<Release>> = RwLock::new(None);

/// Sets the process's release (the host does this once, from its config).
pub fn set_release(r: Release) {
    if let Ok(mut g) = RELEASE.write() {
        *g = Some(r);
    }
}

/// The process's release: what [`set_release`] stored, else the default.
pub fn release() -> Release {
    RELEASE
        .read()
        .ok()
        .and_then(|g| g.clone())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn by_default_only_the_gh_and_git_tools_and_owner_processes_get_the_github_token() {
        let r = Release::default();
        for c in ["gh", "git", "hook", "mcp"] {
            assert!(r.allows("GITHUB_TOKEN", c), "{c}");
        }
        for c in [
            "run_command",
            "job_start",
            "cron",
            "uteke",
            "memory",
            "cora",
            "skills",
            VERIFY,
            TURNEND,
        ] {
            assert!(!r.allows("GITHUB_TOKEN", c), "{c}");
        }
        assert!(
            !r.allows("NGINJEN_API_KEY", "web_search"),
            "unbound: nobody"
        );
    }

    #[test]
    fn an_entry_replaces_the_binding_of_its_name_only() {
        let r = Release::default()
            .with(
                "github_token",
                ["gh".to_string(), "run_command".to_string()],
            )
            .with("NPM_TOKEN", Vec::new());
        assert!(r.allows("GITHUB_TOKEN", "run_command"));
        assert!(!r.allows("GITHUB_TOKEN", "git"), "replaced, not merged");
        assert!(!r.allows("NPM_TOKEN", "run_command"));
        assert_eq!(r.granted_names().collect::<Vec<_>>(), vec!["GITHUB_TOKEN"]);
    }

    #[test]
    fn release_never_narrows_redaction() {
        // Bound to nothing, so it never enters a child; still redacted on egress.
        let set = crate::secrets::SecretSet::collect(
            [("NPM_TOKEN".to_string(), "npm-secret-value-123".to_string())],
            [],
        );
        assert!(Release::default().tools("NPM_TOKEN").is_none());
        assert_eq!(set.redact("got npm-secret-value-123"), "got «NPM_TOKEN»");
    }
}
