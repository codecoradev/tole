//! Config files (#208): LOAD + VALIDATE. Applying the keys (precedence,
//! trust gate at startup) lives in `config_apply`.
//!
//! Two layers share one schema (#433): the project config
//! `.tole/config.toml` and the user config `$CODECORA_HOME/tole/config.toml`
//! (default `~/.codecora/tole/config.toml`). The project layer wins key by
//! key ([`layered`]); the user layer holds a user's global defaults.
//!
//! The project file is untrusted input (it lives in a cloned repo), so the loader
//! is strict: a 64 KiB bounded read, a pre-check that refuses secret-like
//! keys at any depth, trust-preset names validated by the SAME function
//! the `--trust` flag uses, and `deny_unknown_fields` so typos fail
//! loudly. Pure functions only — cwd and paths are parameters, no global
//! state.

pub mod apply;
pub mod discussion_roles;
pub mod hook_filter;
pub mod owner_only;
pub mod profile;
pub mod project_trust;
pub mod routing;
pub mod sandbox;
pub mod session_settings;
pub mod trust;

mod tables;
pub use tables::*;
mod sandbox_config;
pub use sandbox_config::*;
mod secrets_config;
pub use secrets_config::*;
mod profile_config;
pub use profile_config::*;
mod source;
pub use source::*;
mod layering;
pub use layering::*;
mod validate;
pub use validate::*;
mod report;
pub use report::*;
mod render;
pub use render::*;

use serde::Deserialize;
use std::collections::{BTreeMap, BTreeSet};

/// The whole file: flat snake_case keys mirroring the global flags, all
/// optional, plus the single `[mission]` table.
#[derive(Debug, Default, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub model: Option<String>,
    pub base_url: Option<String>,
    pub sessions_dir: Option<String>,
    pub workspace: Option<String>,
    pub plan_mode: Option<bool>,
    pub memory: Option<String>,
    pub trust: Option<Vec<String>>,
    pub allow: Option<Vec<String>>,
    pub mcp_server: Option<Vec<String>>,
    pub no_auto_mcp: Option<bool>,
    pub on_pretool: Option<Vec<String>>,
    pub on_posttool: Option<Vec<String>>,
    pub on_turnend: Option<Vec<String>>,
    pub skill: Option<Vec<String>>,
    pub no_skills: Option<bool>,
    pub system_prompt: Option<String>,
    /// The model's context window in tokens (#440); overrides `/models`.
    pub context_window: Option<u64>,
    /// Fraction of the context window that triggers compaction (#440).
    pub compact_at: Option<f64>,
    /// Turn context compaction off (#440).
    pub no_compaction: Option<bool>,
    /// `reasoning_effort` sent on chat completions (#457):
    /// `minimal | low | medium | high`.
    pub reasoning_effort: Option<String>,
    /// Review gate before `git commit` (#534): `off | warn | block`. A
    /// project file can only keep or tighten the gate: see [`layered`].
    pub review_gate: Option<String>,
    pub mission: Option<MissionConfig>,
    /// Theme and colors (#463); user layer only, see [`TuiConfig`].
    pub tui: Option<TuiConfig>,
    /// Scheduler limits (#579); user layer only, see [`CronConfig`].
    pub cron: Option<CronConfig>,
    /// Discussion request timeout (#786); user layer only, see
    /// [`DiscussionsConfig`].
    pub discussions: Option<DiscussionsConfig>,
    /// Chat and mission request timeout (#1236); user layer only, see
    /// [`ProviderConfig`].
    pub provider: Option<ProviderConfig>,
    /// The sandbox of stdio MCP servers (#601); user layer only, see
    /// [`SandboxConfig`].
    pub sandbox: Option<SandboxConfig>,
    /// The skill curator (#593); user layer only, see [`CuratorConfig`].
    pub curator: Option<CuratorConfig>,
    /// Delegation limits (#585); user layer only, see [`DelegationConfig`].
    pub delegation: Option<DelegationConfig>,
    /// Dormant profiles (#584); user layer only, see [`ProfileConfig`].
    pub profiles: Option<std::collections::BTreeMap<String, ProfileConfig>>,
    /// Webhook routes of `tole serve` (#582); user layer only, see
    /// [`HookConfig`].
    pub hooks: Option<BTreeMap<String, HookConfig>>,
    /// Outbound webhook destinations (#911); user layer only, see
    /// [`WebhookOutConfig`].
    pub webhook_out: Option<BTreeMap<String, WebhookOutConfig>>,
    /// Model routing (#586); tiers and rules from the user layer only, see
    /// [`RouterConfig`].
    pub router: Option<RouterConfig>,
    /// Scoped secret release (#598), see [`SecretBinding`]; a project
    /// layer can only narrow it.
    pub secrets: Option<BTreeMap<String, SecretBinding>>,
    /// Extra sensitive file-name globs (#683), e.g. `["*.secret"]`; the
    /// project layer's list is added to the user's, never replaces it.
    pub sensitive_files: Option<Vec<String>>,
    /// Regexes of secret-shaped values the detector keeps (#597); user
    /// layer only: a repository must not exempt its own values.
    pub redaction_allowlist: Option<Vec<String>>,
    /// Keys (rendered names, e.g. `model`, `mission.verify`) whose value
    /// came from the user layer; set by [`layered`], never by the file.
    #[serde(skip)]
    pub user_keys: BTreeSet<&'static str>,
}

#[cfg(test)]
mod tests;
