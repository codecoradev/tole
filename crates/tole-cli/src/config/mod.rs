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

use serde::Deserialize;
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

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

/// Longest `system_prompt` rendered by `config check`, in chars.
const PROMPT_DISPLAY_CHARS: usize = 80;

fn quote(s: &str) -> String {
    format!("{s:?}")
}

/// Per-key text appended after `[config]` on the key's line (effective
/// value + source, or the "not applied" marker). Keys use the rendered
/// names (`model`, `mission.max_steps`, ...).
pub type Annotations = std::collections::BTreeMap<&'static str, String>;

/// Render `key = value  [config]` lines for every key that is set. List
/// values go one entry per line; unset keys are not listed.
pub fn render(cfg: &Config) -> Vec<String> {
    render_with(cfg, PROJECT_TAG, &Annotations::new())
}

/// Suffix of a project config's `tui` lines in `config check` (#463).
pub const TUI_IGNORED_NOTE: &str =
    "  IGNORED: a project config cannot set the theme, colors or keys (user config only; the theme also TOLE_THEME)";

/// Suffix of a project config's `cron` lines in `config check` (#579).
pub const CRON_IGNORED_NOTE: &str =
    "  IGNORED: a project config cannot set scheduler limits (user config only)";
/// Suffix of a project config's `discussions` lines in `config check` (#786).
pub const DISCUSSIONS_IGNORED_NOTE: &str =
    "  IGNORED: a project config cannot set the discussion request timeout (user config only; also TOLE_DISCUSSION_TIMEOUT_SECS)";
/// Suffix of a project config's `provider` lines in `config check` (#1236).
pub const PROVIDER_IGNORED_NOTE: &str =
    "  IGNORED: a project config cannot set the provider request timeout (user config only; also TOLE_REQUEST_TIMEOUT_SECS)";
/// Suffix of a project config's `sandbox` lines in `config check` (#601).
pub const SANDBOX_IGNORED_NOTE: &str =
    "  IGNORED: a project config cannot set the sandbox (user config only)";
/// Suffix of a project config's `curator` lines in `config check` (#593).
pub const CURATOR_IGNORED_NOTE: &str =
    "  IGNORED: a project config cannot change the skill curator (user config only)";
/// Suffix of a project config's `delegation` lines in `config check` (#585).
pub const DELEGATION_IGNORED_NOTE: &str =
    "  IGNORED: a project config cannot set delegation limits (user config only)";
/// Suffix of a project config's `profiles` line in `config check` (#584).
pub const PROFILES_IGNORED_NOTE: &str =
    "  IGNORED: a project config cannot define profiles (user config only)";

/// Suffix of a project config's `secrets` lines in `config check` (#598).
pub const SECRETS_PROJECT_NOTE: &str =
    "  a project config can only narrow a secret's tools (widening is ignored)";

/// The warning printed at startup when a project config carries `[profiles]`.
pub fn profiles_ignored_warning(shown: &str) -> String {
    format!(
        "ignoring `profiles` in project config {shown}: profiles can only be defined in the \
         user config"
    )
}

/// The warning printed at startup when a project config carries `[hooks]`.
pub fn hooks_ignored_warning(shown: &str) -> String {
    format!(
        "ignoring `hooks` in project config {shown}: webhook routes can only be defined in \
         the user config"
    )
}

/// The warning printed at startup when a project config carries
/// `[webhook_out]` (#911).
pub fn webhook_out_ignored_warning(shown: &str) -> String {
    format!(
        "ignoring `webhook_out` in project config {shown}: outbound webhook destinations can \
         only be defined in the user config"
    )
}

/// The warning printed at startup when a project config carries
/// `redaction_allowlist` (#597).
pub fn redaction_allowlist_ignored_warning(shown: &str) -> String {
    format!(
        "ignoring `redaction_allowlist` in project config {shown}: the secret-shape allowlist \
         can only be set in the user config"
    )
}

/// The warning printed at startup when a project config carries `[tui]`.
pub fn tui_ignored_warning(shown: &str) -> String {
    format!(
        "ignoring `tui` in project config {shown}: the theme, colors and keys can only be set in \
         the user config (the theme also with TOLE_THEME)"
    )
}

/// The `[tag]` on a project config line.
pub const PROJECT_TAG: &str = "config";
/// The `[tag]` on a user config line.
pub const USER_TAG: &str = "user config";

/// [`render`] with the layer's tag and a per-key suffix after it.
pub fn render_with(cfg: &Config, tag: &str, ann: &Annotations) -> Vec<String> {
    let src = tag;
    let mut out = Vec::new();
    let note = |key: &str| {
        if tag == PROJECT_TAG && key.starts_with("tui.") {
            return TUI_IGNORED_NOTE;
        }
        if tag == PROJECT_TAG && key.starts_with("cron.") {
            return CRON_IGNORED_NOTE;
        }
        if tag == PROJECT_TAG && key.starts_with("provider.") {
            return PROVIDER_IGNORED_NOTE;
        }
        if tag == PROJECT_TAG && key.starts_with("discussions.") {
            return DISCUSSIONS_IGNORED_NOTE;
        }
        if tag == PROJECT_TAG && key.starts_with("sandbox.") {
            return SANDBOX_IGNORED_NOTE;
        }
        if tag == PROJECT_TAG && key.starts_with("curator.") {
            return CURATOR_IGNORED_NOTE;
        }
        if tag == PROJECT_TAG && key.starts_with("delegation.") {
            return DELEGATION_IGNORED_NOTE;
        }
        if tag == PROJECT_TAG && key == "profiles" {
            return PROFILES_IGNORED_NOTE;
        }
        ann.get(key).map_or("", String::as_str)
    };
    let scalar = |out: &mut Vec<String>, key: &str, v: Option<String>| {
        if let Some(v) = v {
            out.push(format!("{key} = {v}  [{src}]{}", note(key)));
        }
    };
    let list = |out: &mut Vec<String>, key: &str, v: &Option<Vec<String>>| {
        if let Some(items) = v {
            out.push(format!(
                "{key} = [{} entries]  [{src}]{}",
                items.len(),
                note(key)
            ));
            for i in items {
                out.push(format!("  - {}", quote(i)));
            }
        }
    };
    scalar(&mut out, "model", cfg.model.as_deref().map(quote));
    scalar(&mut out, "base_url", cfg.base_url.as_deref().map(quote));
    scalar(
        &mut out,
        "sessions_dir",
        cfg.sessions_dir.as_deref().map(quote),
    );
    scalar(&mut out, "workspace", cfg.workspace.as_deref().map(quote));
    scalar(&mut out, "plan_mode", cfg.plan_mode.map(|b| b.to_string()));
    scalar(&mut out, "memory", cfg.memory.as_deref().map(quote));
    list(&mut out, "trust", &cfg.trust);
    list(&mut out, "allow", &cfg.allow);
    list(&mut out, "mcp_server", &cfg.mcp_server);
    scalar(
        &mut out,
        "no_auto_mcp",
        cfg.no_auto_mcp.map(|b| b.to_string()),
    );
    list(&mut out, "on_pretool", &cfg.on_pretool);
    list(&mut out, "on_posttool", &cfg.on_posttool);
    list(&mut out, "on_turnend", &cfg.on_turnend);
    list(&mut out, "skill", &cfg.skill);
    list(&mut out, "sensitive_files", &cfg.sensitive_files);
    list(&mut out, "redaction_allowlist", &cfg.redaction_allowlist);
    scalar(&mut out, "no_skills", cfg.no_skills.map(|b| b.to_string()));
    scalar(
        &mut out,
        "system_prompt",
        cfg.system_prompt.as_deref().map(|p| {
            let total = p.chars().count();
            if total > PROMPT_DISPLAY_CHARS {
                let head: String = p.chars().take(PROMPT_DISPLAY_CHARS).collect();
                format!("{}… ({total} chars)", quote(&head))
            } else {
                quote(p)
            }
        }),
    );
    scalar(
        &mut out,
        "context_window",
        cfg.context_window.map(|n| n.to_string()),
    );
    scalar(
        &mut out,
        "compact_at",
        cfg.compact_at.map(|n| n.to_string()),
    );
    scalar(
        &mut out,
        "no_compaction",
        cfg.no_compaction.map(|b| b.to_string()),
    );
    scalar(
        &mut out,
        "reasoning_effort",
        cfg.reasoning_effort.as_deref().map(quote),
    );
    scalar(
        &mut out,
        "review_gate",
        cfg.review_gate.as_deref().map(quote),
    );
    if let Some(t) = &cfg.tui {
        scalar(&mut out, "tui.theme", t.theme.as_deref().map(quote));
        if let Some(c) = &t.colors {
            out.push(format!(
                "tui.colors = [{} entries]  [{src}]{}",
                c.len(),
                note("tui.colors")
            ));
            for (role, spec) in c {
                out.push(format!("  - {role} = {}", quote(spec)));
            }
        }
        if let Some(k) = &t.keys {
            out.push(format!(
                "tui.keys = [{} entries]  [{src}]{}",
                k.len(),
                note("tui.keys")
            ));
            for (action, keys) in k {
                let shown: Vec<String> = keys.to_vec().iter().map(|s| quote(s)).collect();
                out.push(format!("  - {action} = [{}]", shown.join(", ")));
            }
        }
    }
    if let Some(p) = cfg.profiles.as_ref().filter(|p| !p.is_empty()) {
        // Names only: a profile is shown by `config check --profile <name>`.
        out.push(format!(
            "profiles = [{} entries]  [{src}]{}",
            p.len(),
            note("profiles")
        ));
        for name in p.keys() {
            out.push(format!("  - {}", quote(name)));
        }
    }
    if let Some(w) = cfg.webhook_out.as_ref().filter(|w| !w.is_empty()) {
        // Destination names only (#911): never a URL, and never where a
        // secret comes from.
        let ignored = if tag == PROJECT_TAG {
            "  IGNORED: a project config cannot define webhook destinations (user config only)"
        } else {
            ""
        };
        out.push(format!(
            "webhook_out = [{} entries]  [{src}]{ignored}",
            w.len()
        ));
        for name in w.keys() {
            out.push(format!("  - {}", quote(name)));
        }
    }
    if let Some(h) = cfg.hooks.as_ref().filter(|h| !h.is_empty()) {
        // Route names only (#582): never where a key comes from.
        let ignored = if tag == PROJECT_TAG {
            "  IGNORED: a project config cannot define webhook routes (user config only)"
        } else {
            ""
        };
        out.push(format!("hooks = [{} entries]  [{src}]{ignored}", h.len()));
        for name in h.keys() {
            out.push(format!("  - {}", quote(name)));
        }
    }
    if let Some(r) = &cfg.router {
        // Names only (#586): never models or keywords.
        let narrow = if tag == PROJECT_TAG {
            "  a project config can only set enabled = false or narrow allow_tiers (tiers and rules are ignored)"
        } else {
            ""
        };
        let on = if r.enabled == Some(true) { "on" } else { "off" };
        out.push(format!("router = {on}  [{src}]{narrow}"));
        if let Some(a) = &r.allow_tiers {
            out.push(format!("  allow_tiers = [{}]", a.join(", ")));
        }
        if let Some(t) = &r.tiers {
            let set: Vec<&str> = [
                ("light", &t.light),
                ("standard", &t.standard),
                ("heavy", &t.heavy),
            ]
            .into_iter()
            .filter(|(_, m)| m.is_some())
            .map(|(n, _)| n)
            .collect();
            out.push(format!("  tiers = [{}]", set.join(", ")));
        }
        for rule in r.rules.iter().flatten() {
            out.push(format!(
                "  - rule {} -> {}",
                quote(&rule.name),
                quote(&rule.tier)
            ));
        }
    }
    for (name, b) in cfg.secrets.iter().flatten() {
        let key = format!("secrets.{name}");
        let note = if tag == PROJECT_TAG {
            SECRETS_PROJECT_NOTE
        } else {
            ""
        };
        out.push(format!(
            "{key} = [{}]  [{src}]{note}",
            b.tools
                .iter()
                .map(|t| quote(t))
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    if let Some(m) = &cfg.mission {
        scalar(
            &mut out,
            "mission.max_steps",
            m.max_steps.map(|n| n.to_string()),
        );
        scalar(
            &mut out,
            "mission.max_minutes",
            m.max_minutes.map(|n| n.to_string()),
        );
        scalar(
            &mut out,
            "mission.max_tokens",
            m.max_tokens.map(|n| n.to_string()),
        );
        scalar(&mut out, "mission.verify", m.verify.as_deref().map(quote));
        scalar(
            &mut out,
            "mission.verify_timeout",
            m.verify_timeout.map(|n| n.to_string()),
        );
    }
    if let Some(d) = &cfg.discussions {
        scalar(
            &mut out,
            "discussions.request_timeout_secs",
            d.request_timeout_secs.map(|n| n.to_string()),
        );
    }
    if let Some(p) = &cfg.provider {
        scalar(
            &mut out,
            "provider.request_timeout_secs",
            p.request_timeout_secs.map(|n| n.to_string()),
        );
    }
    if let Some(c) = &cfg.cron {
        scalar(
            &mut out,
            "cron.daily_token_cap",
            c.daily_token_cap.map(|n| n.to_string()),
        );
        scalar(
            &mut out,
            "cron.max_concurrent",
            c.max_concurrent.map(|n| n.to_string()),
        );
        scalar(&mut out, "cron.timezone", c.timezone.as_deref().map(quote));
        scalar(
            &mut out,
            "cron.tz_source",
            c.tz_source.map(|s| {
                match s {
                    TzSource::System => "\"system\"",
                    TzSource::OffsetOnly => "\"offset-only\"",
                }
                .to_string()
            }),
        );
    }
    if let Some(s) = &cfg.sandbox {
        scalar(&mut out, "sandbox.mode", s.mode.as_deref().map(quote));
        scalar(&mut out, "sandbox.profile", s.profile.as_deref().map(quote));
        scalar(&mut out, "sandbox.network", s.network.as_deref().map(quote));
        for (name, p) in s.profiles.iter().flatten() {
            let k = |f: &str| format!("sandbox.profiles.{name}.{f}");
            scalar(&mut out, &k("workspace"), p.workspace.as_deref().map(quote));
            scalar(&mut out, &k("network"), p.network.as_deref().map(quote));
            list(&mut out, &k("read"), &p.read);
            list(&mut out, &k("write"), &p.write);
        }
        for (name, sv) in s.servers.iter().flatten() {
            scalar(
                &mut out,
                &format!("sandbox.servers.{name}.profile"),
                sv.profile.as_deref().map(quote),
            );
        }
        if let Some(c) = &s.consumers {
            let n = |v: Option<u64>| v.map(|n| n.to_string());
            for (key, v) in [
                ("sandbox.consumers.run_command", &c.run_command),
                ("sandbox.consumers.job_start", &c.job_start),
                ("sandbox.consumers.verify", &c.verify),
                ("sandbox.consumers.turnend", &c.turnend),
                ("sandbox.consumers.cron", &c.cron),
                ("sandbox.consumers.network", &c.network),
            ] {
                scalar(&mut out, key, v.as_deref().map(quote));
            }
            for (key, v) in [
                ("sandbox.consumers.cpu_secs", c.cpu_secs),
                ("sandbox.consumers.memory_mb", c.memory_mb),
                ("sandbox.consumers.processes", c.processes),
                ("sandbox.consumers.file_size_mb", c.file_size_mb),
            ] {
                scalar(&mut out, key, n(v));
            }
        }
    }
    if let Some(c) = &cfg.curator {
        scalar(
            &mut out,
            "curator.enabled",
            c.enabled.map(|b| b.to_string()),
        );
        for (key, v) in [
            ("curator.stale_days", c.stale_days),
            ("curator.archive_days", c.archive_days),
            ("curator.grace_days", c.grace_days),
        ] {
            scalar(&mut out, key, v.map(|n| n.to_string()));
        }
    }
    if let Some(d) = &cfg.delegation {
        let n = |v: Option<u64>| v.map(|n| n.to_string());
        scalar(
            &mut out,
            "delegation.max_depth",
            n(d.max_depth.map(u64::from)),
        );
        scalar(
            &mut out,
            "delegation.max_live_processes",
            n(d.max_live_processes.map(u64::from)),
        );
        scalar(&mut out, "delegation.goal_budget", n(d.goal_budget));
        scalar(
            &mut out,
            "delegation.goal_steps",
            n(d.goal_steps.map(u64::from)),
        );
        scalar(&mut out, "delegation.goal_minutes", n(d.goal_minutes));
        for (key, v) in [
            ("min_free_memory_mb", d.min_free_memory_mb),
            ("child_memory_mb", d.child_memory_mb),
            ("min_free_disk_mb", d.min_free_disk_mb),
            ("min_free_pids_percent", d.min_free_pids_percent),
            ("spawn_interval_secs", d.spawn_interval_secs),
            ("machine_max_live", d.machine_max_live.map(u64::from)),
            ("child_nice", d.child_nice.map(u64::from)),
        ] {
            scalar(&mut out, &format!("delegation.{key}"), n(v));
        }
        scalar(
            &mut out,
            "delegation.max_load_per_cpu",
            d.max_load_per_cpu.map(|v| v.to_string()),
        );
        let mode = d.approvals.map(|m| format!("{m:?}").to_ascii_lowercase());
        scalar(&mut out, "delegation.approvals", mode);
        scalar(&mut out, "delegation.approval_secs", n(d.approval_secs));
        scalar(
            &mut out,
            "delegation.approval_level_secs",
            n(d.approval_level_secs),
        );
    }
    out
}

/// `tole config check`: returns the text to print, or the error. `cwd`,
/// `explicit` and the user config path are parameters (no global state).
/// `annotate` supplies the per-key suffixes from the shared resolution
/// over the LAYERED config (see `config_apply::check`), so a user key a
/// project key overrides shows the project as its effective source.
pub fn check(
    cwd: &Path,
    explicit: Option<&Path>,
    user: Option<&Path>,
    annotate: &dyn Fn(&Config) -> Annotations,
) -> Result<String, ConfigError> {
    let user_cfg = match user {
        Some(p) if p.exists() => Some((p.to_path_buf(), load(p)?)),
        _ => None,
    };
    let project = match discover(cwd, explicit) {
        Some(path) => Some((path.clone(), load(&path)?)),
        None => None,
    };
    let none = Config::default();
    let merged = layered(
        project.as_ref().map_or(&none, |(_, c)| c),
        user_cfg.as_ref().map_or(&none, |(_, c)| c),
    );
    let ann = annotate(&merged);
    let mut lines = Vec::new();
    let section = |lines: &mut Vec<String>, cfg: &Config, tag: &str| {
        let rendered = render_with(cfg, tag, &ann);
        if rendered.is_empty() {
            lines.push("(valid; no keys set)".to_string());
        } else {
            lines.extend(rendered);
        }
    };
    match (&user_cfg, user) {
        (Some((path, cfg)), _) => {
            let shown = path.canonicalize().unwrap_or(path.clone());
            lines.push(format!("user config: {}", shown.display()));
            section(&mut lines, cfg, USER_TAG);
        }
        (None, Some(p)) => lines.push(format!("no user config at {}", p.display())),
        (None, None) => lines.push("no user config (set CODECORA_HOME or HOME)".into()),
    }
    match &project {
        Some((path, cfg)) => {
            let shown = path.canonicalize().unwrap_or(path.clone());
            lines.push(format!("config: {}", shown.display()));
            section(&mut lines, cfg, PROJECT_TAG);
        }
        None => lines.push(format!("no {CONFIG_REL_PATH} in {}", cwd.display())),
    }
    // The sandbox a session would use: only the user layer feeds it.
    if let Some(s) = &merged.sandbox {
        lines.extend(sandbox::effective_lines(s));
    }
    lines.push(
        "note: precedence is flag > env > config > user config > default; a higher layer \
         replaces a whole list key, flags can only turn boolean keys on, and --no-config \
         ignores both files."
            .into(),
    );
    Ok(lines.join("\n"))
}

#[cfg(test)]
mod tests;
