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

use serde::Deserialize;
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::io::Read;
use std::path::{Path, PathBuf};
use toml::de::{DeTable, DeValue};
use toml::Spanned;

/// Hard cap on the config file size (64 KiB).
pub const MAX_CONFIG_BYTES: u64 = 64 * 1024;

/// Project-relative location of the config file.
pub const CONFIG_REL_PATH: &str = ".tole/config.toml";

/// File name of the user config under the tole data dir (#433).
pub const USER_CONFIG_FILE: &str = "config.toml";

/// Where the user config lives: `$CODECORA_HOME/tole/config.toml`, default
/// `~/.codecora/tole/config.toml` (next to the config trust store). `None`
/// when neither `CODECORA_HOME` nor a home directory is set.
pub fn user_config_path() -> Option<PathBuf> {
    tole_core::paths::tole_data_dir().map(|d| d.join(USER_CONFIG_FILE))
}

/// The `[mission]` table. Types mirror the `tole mission` flags
/// (`--max-steps/--max-minutes/--max-tokens` are `Option<u64>`,
/// `--verify` an `Option<String>`, `--verify-timeout` a `u64` with
/// default 300 — here optional like every key).
#[derive(Debug, Default, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MissionConfig {
    pub max_steps: Option<u64>,
    pub max_minutes: Option<u64>,
    pub max_tokens: Option<u64>,
    pub verify: Option<String>,
    pub verify_timeout: Option<u64>,
}

/// One `[tui.keys]` value: a key spec or a list of them.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(untagged)]
pub enum KeyList {
    One(String),
    Many(Vec<String>),
}

impl KeyList {
    pub fn to_vec(&self) -> Vec<String> {
        match self {
            KeyList::One(s) => vec![s.clone()],
            KeyList::Many(v) => v.clone(),
        }
    }
}

/// The `[tui]` table (#463): theme name, per-role color overrides and key
/// bindings. USER LAYER (and, for the theme, the environment) ONLY: a project
/// config may carry the table (so the file still parses) but [`layered`]
/// drops it, because a cloned repository must not control how the approval
/// prompt looks or which keys do what. Values are validated by `tole-tui`
/// when the theme / keymap is built; a bad value falls back to the default
/// with a startup note and never aborts, so only the TOML types are checked
/// here.
#[derive(Debug, Default, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TuiConfig {
    /// `default | light | mono`.
    pub theme: Option<String>,
    /// Role name -> style spec, e.g. `tool = "blue bold"`.
    pub colors: Option<std::collections::BTreeMap<String, String>>,
    /// Action name -> key spec(s), e.g. `toggle_tools = ["ctrl+o", "alt+o"]`.
    pub keys: Option<std::collections::BTreeMap<String, KeyList>>,
}

/// The `[cron]` table (#579). USER LAYER ONLY, like [`TuiConfig`]: the
/// scheduler is global (one job store per user), so a cloned repository's
/// config must not raise the spending ceiling of every job.
#[derive(Debug, Default, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CronConfig {
    /// Tokens all agent runs of one UTC day may spend together; past it
    /// agent runs settle `budget` without calling the provider.
    pub daily_token_cap: Option<u64>,
    /// Runs one tick (`tole cron tick`, the `tole serve --cron` ticker)
    /// executes at once, 1 to [`MAX_CRON_CONCURRENT`] (default 4).
    pub max_concurrent: Option<usize>,
    /// The time zone of a job saved without `--timezone` (#580): `UTC`, a
    /// fixed offset like `+07:00` or an IANA name. Unset: such a job keeps
    /// plain UTC.
    pub timezone: Option<String>,
    /// Where IANA zone names are resolved (#580) [default: `system` on
    /// unix, else `offset-only`].
    pub tz_source: Option<TzSource>,
}

/// One `[hooks.<route>]` table (#582): a `POST /hooks/<route>` on `tole
/// serve`. USER LAYER ONLY: a cloned repository must not open an endpoint
/// or change one. The route key itself never lives here: only the name of
/// the environment variable or the path of the file that holds it.
#[derive(Debug, Default, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HookConfig {
    /// Environment variable holding the HMAC key.
    pub hmac_key_env: Option<String>,
    /// File holding the HMAC key (a regular file, not a link, at most 4 KiB).
    pub hmac_key_file: Option<String>,
    /// No signature check; only when `tole serve` binds a loopback address,
    /// and only for loopback clients.
    pub insecure_loopback: Option<bool>,
    /// Deliveries accepted per minute (default 30, 1 to 600).
    pub rate_per_minute: Option<u32>,
    /// Largest body (default 262144, at most 1048576).
    pub max_body_bytes: Option<u64>,
    /// The job a matching delivery fires (#582 part 2). Unset: deliveries
    /// are only verified and recorded.
    pub cron_job: Option<String>,
    /// Events admitted (`X-GitHub-Event` / `X-Tole-Event`); unset: all.
    pub events: Option<Vec<String>>,
    /// The filter a delivery's JSON body must pass.
    pub filter: Option<crate::hook_filter::FilterConfig>,
    /// `{dot.path}` template of the context the fired run sees.
    pub template: Option<String>,
    /// Let the fired run use the job's Write tools (default: ReadOnly).
    pub allow_writes: Option<bool>,
}

/// `cron.tz_source`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum TzSource {
    /// The host's zoneinfo (`$TZDIR`, else `/usr/share/zoneinfo`), read at
    /// run time; no zone data in the binary.
    System,
    /// UTC and fixed offsets only.
    OffsetOnly,
}

/// Upper bound of `cron.max_concurrent`.
pub const MAX_CRON_CONCURRENT: usize = 64;

/// The `[delegation]` table (#585). USER LAYER ONLY, like [`CronConfig`]:
/// it sets how deep and how wide delegation may go and the default budget
/// of a goal, so a cloned repository must not raise it.
#[derive(Debug, Default, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DelegationConfig {
    /// Depth below the orchestrator, 1 to 3 (default 2).
    pub max_depth: Option<u32>,
    /// Machine guard: delegated processes live at once before spawns queue.
    pub max_live_processes: Option<u32>,
    /// A goal's default budget: prompt + completion units of the provider.
    pub goal_budget: Option<u64>,
    /// A goal's default provider steps.
    pub goal_steps: Option<u32>,
    /// A goal's default wall-clock minutes.
    pub goal_minutes: Option<u64>,
    /// Resource guard (`tole_core::resource_guard`): memory that must stay
    /// free after a child is reserved, in MiB (default 1024).
    pub min_free_memory_mb: Option<u64>,
    /// MiB reserved per child started but not yet ramped up (default 512).
    pub child_memory_mb: Option<u64>,
    /// Highest one-minute load per CPU at which a child starts (default 1.5).
    pub max_load_per_cpu: Option<f64>,
    /// MiB that must stay free on the data dir, workspace and worktree
    /// filesystems (default 2048).
    pub min_free_disk_mb: Option<u64>,
    /// Percent of the process-id limit that must stay free (default 10).
    pub min_free_pids_percent: Option<u64>,
    /// Least seconds between two child starts (default 2).
    pub spawn_interval_secs: Option<u64>,
    /// Delegated children live at once on the machine, all goals and
    /// sessions together (default max(2, CPUs)).
    pub machine_max_live: Option<u32>,
    /// Niceness of a delegated child, 0 to 19 (default 10).
    pub child_nice: Option<u32>,
}

impl DelegationConfig {
    /// The resource-guard thresholds this table sets (defaults for the
    /// rest); an out-of-bounds value is an error naming its key.
    pub fn resource_thresholds(&self) -> Result<tole_core::resource_guard::Thresholds, String> {
        tole_core::resource_guard::Thresholds::with(&tole_core::resource_guard::Overrides {
            min_free_memory_mb: self.min_free_memory_mb,
            child_memory_mb: self.child_memory_mb,
            max_load_per_cpu: self.max_load_per_cpu,
            min_free_disk_mb: self.min_free_disk_mb,
            min_free_pids_percent: self.min_free_pids_percent,
            spawn_interval_secs: self.spawn_interval_secs,
            machine_max_live: self.machine_max_live,
            child_nice: self.child_nice,
        })
    }
}

/// `[router]` (#586): the RuleRouter, OFF unless `enabled = true`. Tiers and
/// rules come from the user config only; a project config can only turn
/// it off or narrow `allow_tiers` (see [`layered`]). Checked by
/// [`crate::routing`].
#[derive(Debug, Default, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RouterConfig {
    pub enabled: Option<bool>,
    /// The tiers rules may pick (unset: all).
    pub allow_tiers: Option<Vec<String>>,
    /// Models of runs without a profile (a profile's own come from
    /// `profiles.<name>.models`).
    pub tiers: Option<RouterTiers>,
    pub rules: Option<Vec<RouterRule>>,
}

/// `[router.tiers]`: a model name per tier.
#[derive(Debug, Default, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RouterTiers {
    pub light: Option<String>,
    pub standard: Option<String>,
    pub heavy: Option<String>,
}

/// One `[[router.rules]]` entry; every set condition must hold.
#[derive(Debug, Default, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RouterRule {
    pub name: String,
    /// `light | standard | heavy`.
    pub tier: String,
    /// `chat | run | mission | cron | delegate`.
    pub kinds: Option<Vec<String>>,
    pub min_chars: Option<usize>,
    pub max_chars: Option<usize>,
    /// `read_only | write | destructive`.
    pub min_risk: Option<String>,
    pub max_risk: Option<String>,
    /// Profiles the rule applies to (unset: runs without a profile only).
    pub profiles: Option<Vec<String>>,
    pub jobs: Option<Vec<String>>,
    pub budget_below: Option<u64>,
    pub keywords: Option<Vec<String>>,
}

/// `[profiles.<name>.models]` (#584): model names per task-weight tier.
/// `default` names the tier a session uses unless something picks another
/// (the `[router]` of #586, when enabled, for rules that name the profile).
#[derive(Debug, Default, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelTiers {
    pub light: Option<String>,
    pub standard: Option<String>,
    pub heavy: Option<String>,
    /// Consulted, never executes (#587).
    pub advisor: Option<String>,
    /// `light | standard | heavy`; default `standard`.
    pub default: Option<String>,
}

/// The executing tiers a `default` may name (`advisor` never executes).
pub const EXEC_TIERS: [&str; 3] = ["light", "standard", "heavy"];

impl ModelTiers {
    /// The model of an executing tier, when that tier is set.
    pub fn tier(&self, name: &str) -> Option<&str> {
        match name {
            "light" => self.light.as_deref(),
            "standard" => self.standard.as_deref(),
            "heavy" => self.heavy.as_deref(),
            _ => None,
        }
        .filter(|m| !m.trim().is_empty())
    }

    /// The `default` tier's name (`standard` when unset).
    pub fn default_tier(&self) -> &str {
        self.default.as_deref().map(str::trim).unwrap_or("standard")
    }
}

/// One `[profiles.<name>]` table (#584): a dormant persona. It holds
/// configuration only; selecting it (`--profile <name>`) is the only thing
/// that reads its persona file, and nothing is ever started for it.
/// USER LAYER ONLY, like `[tui]`: a project config may not define who the
/// agent is or what it may do without asking (see [`layered`]).
#[derive(Debug, Default, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProfileConfig {
    /// Persona file (`~/` expanded; relative to the user config's directory).
    pub system_prompt_file: Option<String>,
    /// Tool patterns pre-authorized for this profile (same syntax as `allow`).
    pub allow: Option<Vec<String>>,
    /// uteke room of this profile's memory loop (default `profile-<name>`).
    pub memory_room: Option<String>,
    /// Skill files preloaded for this profile (same syntax as `skill`).
    pub skills: Option<Vec<String>>,
    /// NAMES of the environment secrets this profile may be granted
    /// (never values; #598 releases them, #585 intersects with the parent).
    pub grants: Option<Vec<String>>,
    pub models: Option<ModelTiers>,
    /// `consult_advisor` calls per run (#587; default 3, 1 to 20). Only with
    /// `models.advisor` set.
    pub advisor_max_uses: Option<u32>,
}

/// One `[secrets]` entry (#598): the tools (and tole launchers) whose
/// child processes may receive this secret. The KEY is the environment
/// variable's NAME; a value never belongs in a config file.
#[derive(Debug, Default, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SecretBinding {
    pub tools: Vec<String>,
}

/// A `[secrets]` key: a secret-shaped environment variable NAME.
fn check_secret(name: &str, b: &SecretBinding) -> Result<(), String> {
    // Never echo the key or an entry: either may be a value pasted by mistake.
    if !valid_grant_name(name) || !tole_core::secrets::is_secret_name(name) {
        return Err(
            "secrets: every key must be the NAME of a secret environment variable \
                    ([A-Z0-9_], containing TOKEN, SECRET, API_KEY, PASSWORD, ...), e.g. \
                    GITHUB_TOKEN; other variables already reach every child"
                .into(),
        );
    }
    let tool_ok = |t: &str| {
        !t.is_empty()
            && t.len() <= 64
            && t.bytes()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'_')
    };
    if !b.tools.iter().all(|t| tool_ok(t)) {
        return Err(format!(
            "secrets.{name}.tools: every entry must be a tool name of a-z, 0-9 or `_` \
             (e.g. \"gh\", \"git\", \"run_command\")"
        ));
    }
    Ok(())
}

/// The consumers a layer leaves `name` bound to: its own entry, else the
/// built-in default (`GITHUB_TOKEN` for gh, git, hook and mcp).
fn bound_tools(layer: &Option<BTreeMap<String, SecretBinding>>, name: &str) -> Vec<String> {
    match layer.as_ref().and_then(|m| m.get(name)) {
        Some(b) => b.tools.clone(),
        None => tole_core::secret_release::Release::default()
            .tools(name)
            .map(|t| t.iter().cloned().collect())
            .unwrap_or_default(),
    }
}

/// The `[secrets]` of the layered config (#598): the user layer's entries,
/// where a project entry may only NARROW a binding (its tools ∩ the user's
/// or the default's). A project file comes from a cloned repository; it
/// must never hand a secret to a tool the user did not.
fn layered_secrets(
    project: &Option<BTreeMap<String, SecretBinding>>,
    user: &Option<BTreeMap<String, SecretBinding>>,
) -> Option<BTreeMap<String, SecretBinding>> {
    let mut out = user.clone().unwrap_or_default();
    for (name, b) in project.iter().flatten() {
        let allowed = bound_tools(user, name);
        let tools = b
            .tools
            .iter()
            .filter(|t| allowed.contains(t))
            .cloned()
            .collect();
        out.insert(name.clone(), SecretBinding { tools });
    }
    (project.is_some() || user.is_some()).then_some(out)
}

/// One warning per project `[secrets]` entry that tries to WIDEN a binding
/// (those tools are refused), naming the remedy.
pub fn secrets_widening_warnings(project: &Config, user: &Config, shown: &str) -> Vec<String> {
    let mut out = Vec::new();
    for (name, b) in project.secrets.iter().flatten() {
        let allowed = bound_tools(&user.secrets, name);
        let refused: Vec<&str> = b
            .tools
            .iter()
            .filter(|t| !allowed.contains(t))
            .map(String::as_str)
            .collect();
        if !refused.is_empty() {
            out.push(format!(
                "ignoring the grant of {name} to {} in project config {shown}: a project config \
                 can only narrow a secret's tools. To grant it, add the tools to `{name}` under \
                 [secrets] in your user config",
                refused.join(", ")
            ));
        }
    }
    out
}

impl Config {
    /// The secret release this config sets (#598): the defaults, with each
    /// `[secrets]` entry replacing its name's binding.
    pub fn release(&self) -> tole_core::secret_release::Release {
        let mut r = tole_core::secret_release::Release::default();
        for (name, b) in self.secrets.iter().flatten() {
            r = r.with(name, b.tools.iter().cloned());
        }
        r
    }
}

/// Profile names: `[a-z0-9_-]`, 1 to 64 chars, not starting with `-`
/// (they become `--profile` values and the `profile-<name>` room).
pub fn valid_profile_name(name: &str) -> bool {
    (1..=64).contains(&name.len())
        && !name.starts_with('-')
        && name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'-')
}

/// An environment variable NAME as a grant: `[A-Z0-9_]`, not starting with
/// a digit. Anything else (a value pasted by mistake) is refused.
fn valid_grant_name(name: &str) -> bool {
    !name.is_empty()
        && !name.as_bytes()[0].is_ascii_digit()
        && name
            .bytes()
            .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit() || b == b'_')
}

fn check_profile(name: &str, p: &ProfileConfig) -> Result<(), String> {
    if !valid_profile_name(name) {
        return Err(format!(
            "profile name `{name}` must be 1-64 chars of a-z, 0-9, `_` or `-`, not starting with `-`"
        ));
    }
    if let Some(room) = &p.memory_room {
        let ok = !room.is_empty()
            && !room.starts_with('-')
            && room
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'));
        if !ok {
            return Err(format!(
                "profiles.{name}.memory_room must be [a-zA-Z0-9._-], not starting with `-`"
            ));
        }
    }
    // Never echo the entry: it may be a value pasted by mistake.
    if p.grants.iter().flatten().any(|g| !valid_grant_name(g)) {
        return Err(format!(
            "profiles.{name}.grants: every entry must be an environment variable NAME              ([A-Z0-9_]), never a value"
        ));
    }
    // The advisor tier (#587): a model that is consulted, never executes.
    let advisor = p.models.as_ref().and_then(|m| m.advisor.as_deref());
    if advisor.is_some_and(|a| a.trim().is_empty()) {
        return Err(format!(
            "profiles.{name}.models.advisor is blank: name the advisor model or remove the key"
        ));
    }
    if let Some(n) = p.advisor_max_uses {
        let max = tole_core::advisor::MAX_USES_CEILING;
        if !(1..=max).contains(&n) {
            return Err(format!(
                "profiles.{name}.advisor_max_uses must be 1 to {max}, got {n}"
            ));
        }
        if advisor.is_none() {
            return Err(format!(
                "profiles.{name}.advisor_max_uses is set but profiles.{name}.models.advisor is \
                 not set: declare the advisor model or drop advisor_max_uses"
            ));
        }
    }
    if let Some(m) = &p.models {
        let tier = m.default_tier();
        if !EXEC_TIERS.contains(&tier) {
            return Err(format!(
                "profiles.{name}.models.default must be light, standard or heavy, got {tier:?}"
            ));
        }
        if m.tier(tier).is_none() {
            return Err(format!(
                "profiles.{name}.models.default is {tier:?} but profiles.{name}.models.{tier} is not set"
            ));
        }
    }
    Ok(())
}

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
    /// Delegation limits (#585); user layer only, see [`DelegationConfig`].
    pub delegation: Option<DelegationConfig>,
    /// Dormant profiles (#584); user layer only, see [`ProfileConfig`].
    pub profiles: Option<std::collections::BTreeMap<String, ProfileConfig>>,
    /// Webhook routes of `tole serve` (#582); user layer only, see
    /// [`HookConfig`].
    pub hooks: Option<BTreeMap<String, HookConfig>>,
    /// Model routing (#586); tiers and rules from the user layer only, see
    /// [`RouterConfig`].
    pub router: Option<RouterConfig>,
    /// Scoped secret release (#598), see [`SecretBinding`]; a project
    /// layer can only narrow it.
    pub secrets: Option<BTreeMap<String, SecretBinding>>,
    /// Keys (rendered names, e.g. `model`, `mission.verify`) whose value
    /// came from the user layer; set by [`layered`], never by the file.
    #[serde(skip)]
    pub user_keys: BTreeSet<&'static str>,
}

/// A config problem, rendered as `<path>:<line>:<col>: <message>` when a
/// source position is known, `<path>: <message>` otherwise.
#[derive(Debug, Clone, PartialEq)]
pub struct ConfigError {
    pub path: PathBuf,
    pub line_col: Option<(usize, usize)>,
    pub message: String,
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.line_col {
            Some((l, c)) => write!(f, "{}:{l}:{c}: {}", self.path.display(), self.message),
            None => write!(f, "{}: {}", self.path.display(), self.message),
        }
    }
}

impl std::error::Error for ConfigError {}

/// 1-based line/column of a byte offset (column counted in chars).
fn line_col(src: &str, offset: usize) -> (usize, usize) {
    let offset = offset.min(src.len());
    let head = src.get(..offset).unwrap_or_else(|| {
        // Offset inside a multi-byte char: back up to a boundary.
        let mut o = offset;
        while !src.is_char_boundary(o) {
            o -= 1;
        }
        &src[..o]
    });
    let line = head.matches('\n').count() + 1;
    let col = head.rsplit('\n').next().map_or(0, |l| l.chars().count()) + 1;
    (line, col)
}

/// Where the config applies: an explicit path wins; otherwise
/// `<cwd>/.tole/config.toml` if present. No parent walk. `None` = no
/// config. An explicit path is returned as-is even when missing so the
/// load reports the failure instead of silently ignoring a typo.
pub fn discover(cwd: &Path, explicit: Option<&Path>) -> Option<PathBuf> {
    if let Some(p) = explicit {
        return Some(p.to_path_buf());
    }
    let p = cwd.join(CONFIG_REL_PATH);
    p.exists().then_some(p)
}

/// Read at most `MAX_CONFIG_BYTES` (+1 to detect overflow) — never the
/// whole file — and decode as UTF-8.
pub fn read_source(path: &Path) -> Result<String, ConfigError> {
    let err = |message: String| ConfigError {
        path: path.to_path_buf(),
        line_col: None,
        message,
    };
    let file = std::fs::File::open(path).map_err(|e| err(format!("cannot read config: {e}")))?;
    let mut buf = Vec::new();
    file.take(MAX_CONFIG_BYTES + 1)
        .read_to_end(&mut buf)
        .map_err(|e| err(format!("cannot read config: {e}")))?;
    if buf.len() as u64 > MAX_CONFIG_BYTES {
        return Err(err(format!(
            "config file is larger than the {} KiB limit",
            MAX_CONFIG_BYTES / 1024
        )));
    }
    String::from_utf8(buf).map_err(|_| err("config file is not valid UTF-8".to_string()))
}

/// Does this key name look like a secret? Case-insensitive substring
/// match over `secret`, `password`, `token`, `api_key` (`-` ≙ `_`).
/// The schema keys that legitimately contain such a word are
/// `mission.max_tokens` and `cron.daily_token_cap` (budgets, not
/// credentials); they are exempted by exact position in
/// [`check_forbidden`], never by name alone.
fn is_secret_name(key: &str) -> bool {
    let k = key.to_ascii_lowercase().replace('-', "_");
    ["secret", "password", "token", "api_key"]
        .iter()
        .any(|w| k.contains(w))
}

/// Walk the spanned tree; the first secret-like key (any depth, through
/// arrays of tables too) is a hard error naming the key.
fn check_forbidden(
    table: &DeTable<'_>,
    parent: Option<&str>,
    src: &str,
    path: &Path,
) -> Result<(), ConfigError> {
    for (k, v) in table.iter() {
        let name: &str = k.get_ref();
        let allowed = matches!(
            (parent, name),
            (Some("mission"), "max_tokens")
                | (Some("cron"), "daily_token_cap")
                | (None, "secrets")
                // `[secrets]` keys are secret NAMES; their values are checked
                // below and by `check_secret` (never a value).
                | (Some("secrets"), _)
        );
        if !allowed && is_secret_name(name) {
            return Err(ConfigError {
                path: path.to_path_buf(),
                line_col: Some(line_col(src, k.span().start)),
                message: format!(
                    "forbidden key `{name}`: secrets must never live in a config file \
                     (keep credentials in the environment, e.g. TOLE_API_KEY)"
                ),
            });
        }
        check_value(v.get_ref(), name, src, path)?;
    }
    Ok(())
}

fn check_value(v: &DeValue<'_>, key: &str, src: &str, path: &Path) -> Result<(), ConfigError> {
    match v {
        DeValue::Table(t) => check_forbidden(t, Some(key), src, path),
        DeValue::Array(a) => {
            for item in a.iter() {
                check_value(item.get_ref(), key, src, path)?;
            }
            Ok(())
        }
        _ => Ok(()),
    }
}

/// Validate top-level `trust` entries with the shared `--trust` rule
/// ([`crate::trust::expand_trust`]). Non-string entries are left to the
/// typed deserialization for a type error.
fn check_trust(table: &DeTable<'_>, src: &str, path: &Path) -> Result<(), ConfigError> {
    let Some(entry) = table.get("trust") else {
        return Ok(());
    };
    let DeValue::Array(items) = entry.get_ref() else {
        return Ok(());
    };
    for item in items.iter() {
        if let DeValue::String(s) = item.get_ref() {
            if let Err(e) = crate::trust::expand_trust(&[s.to_string()]) {
                return Err(ConfigError {
                    path: path.to_path_buf(),
                    line_col: Some(line_col(src, Spanned::span(item).start)),
                    message: format!("{e}"),
                });
            }
        }
    }
    Ok(())
}

/// Parse + validate config text. `path` is only used in error messages.
pub fn parse(src: &str, path: &Path) -> Result<Config, ConfigError> {
    let toml_err = |e: toml::de::Error| ConfigError {
        path: path.to_path_buf(),
        line_col: e.span().map(|s| line_col(src, s.start)),
        message: e.message().to_string(),
    };
    let doc = DeTable::parse(src).map_err(toml_err)?;
    check_forbidden(doc.get_ref(), None, src, path)?;
    check_trust(doc.get_ref(), src, path)?;
    let cfg = toml::from_str::<Config>(src).map_err(toml_err)?;
    let range = |message: String| ConfigError {
        path: path.to_path_buf(),
        line_col: None,
        message,
    };
    if let Some(at) = cfg.compact_at {
        if !(at > 0.0 && at <= 1.0) {
            return Err(range(format!(
                "compact_at must be greater than 0 and at most 1, got {at}"
            )));
        }
    }
    if let Some(level) = &cfg.reasoning_effort {
        tole_core::openai::parse_reasoning_effort(level)
            .map_err(|e| range(format!("reasoning_effort: {e}")))?;
    }
    if let Some(v) = &cfg.review_gate {
        if gate_strictness(v).is_none() {
            return Err(range(format!(
                "review_gate must be off, warn or block, got {v:?}"
            )));
        }
    }
    if cfg.context_window == Some(0) {
        return Err(range(
            "context_window must be a positive number of tokens".into(),
        ));
    }
    for (name, p) in cfg.profiles.iter().flatten() {
        check_profile(name, p).map_err(range)?;
    }
    for (name, b) in cfg.secrets.iter().flatten() {
        check_secret(name, b).map_err(range)?;
    }
    if let Some(r) = &cfg.router {
        crate::routing::compile(r).map_err(range)?;
    }
    if let Some(n) = cfg.cron.as_ref().and_then(|c| c.max_concurrent) {
        if !(1..=MAX_CRON_CONCURRENT).contains(&n) {
            return Err(range(format!(
                "cron.max_concurrent must be 1 to {MAX_CRON_CONCURRENT}, got {n}"
            )));
        }
    }
    if let Some(d) = &cfg.delegation {
        let ceiling = tole_core::delegation::MAX_DEPTH_CEILING;
        if d.max_depth.is_some_and(|n| !(1..=ceiling).contains(&n)) {
            return Err(range(format!(
                "delegation.max_depth must be 1 to {ceiling}"
            )));
        }
        let zero = [
            ("max_live_processes", d.max_live_processes.map(u64::from)),
            ("goal_budget", d.goal_budget),
            ("goal_steps", d.goal_steps.map(u64::from)),
            ("goal_minutes", d.goal_minutes),
        ];
        if let Some((k, _)) = zero.iter().find(|(_, v)| *v == Some(0)) {
            return Err(range(format!("delegation.{k} must be at least 1")));
        }
        d.resource_thresholds().map_err(range)?;
    }
    Ok(cfg)
}

/// Read (bounded) and validate the config file at `path`.
pub fn load(path: &Path) -> Result<Config, ConfigError> {
    let src = read_source(path)?;
    parse(&src, path)
}

/// Strictness of a `review_gate` value (`block` > `warn` > `off`); `None`
/// when it is not one of the three.
fn gate_strictness(v: &str) -> Option<u8> {
    match v.trim().to_ascii_lowercase().as_str() {
        "off" => Some(0),
        "warn" => Some(1),
        "block" => Some(2),
        _ => None,
    }
}

/// Merge the two layers key by key: the project value wins when it is set
/// (by the same rules the precedence functions use: a blank string or an
/// empty list counts as unset), else the user value. Lists are never
/// merged across layers. The result records which keys came from the user
/// layer in [`Config::user_keys`].
pub fn layered(project: &Config, user: &Config) -> Config {
    let mut from_user = BTreeSet::new();
    fn pick<T: Clone>(
        key: &'static str,
        p: &Option<T>,
        u: &Option<T>,
        set: fn(&T) -> bool,
        from_user: &mut BTreeSet<&'static str>,
    ) -> Option<T> {
        match (p, u) {
            (Some(p), _) if set(p) => Some(p.clone()),
            (_, Some(u)) if set(u) => {
                from_user.insert(key);
                Some(u.clone())
            }
            _ => p.clone(),
        }
    }
    let text = |v: &String| !v.trim().is_empty();
    let list = |v: &Vec<String>| !v.is_empty();
    fn any<T>(_: &T) -> bool {
        true
    }
    let pm = project.mission.clone().unwrap_or_default();
    let um = user.mission.clone().unwrap_or_default();
    let u = &mut from_user;
    let mission = MissionConfig {
        max_steps: pick("mission.max_steps", &pm.max_steps, &um.max_steps, any, u),
        max_minutes: pick(
            "mission.max_minutes",
            &pm.max_minutes,
            &um.max_minutes,
            any,
            u,
        ),
        max_tokens: pick("mission.max_tokens", &pm.max_tokens, &um.max_tokens, any, u),
        verify: pick("mission.verify", &pm.verify, &um.verify, text, u),
        verify_timeout: pick(
            "mission.verify_timeout",
            &pm.verify_timeout,
            &um.verify_timeout,
            any,
            u,
        ),
    };
    let mut out = Config {
        model: pick("model", &project.model, &user.model, text, u),
        base_url: pick("base_url", &project.base_url, &user.base_url, text, u),
        sessions_dir: pick(
            "sessions_dir",
            &project.sessions_dir,
            &user.sessions_dir,
            text,
            u,
        ),
        workspace: pick("workspace", &project.workspace, &user.workspace, text, u),
        plan_mode: pick("plan_mode", &project.plan_mode, &user.plan_mode, any, u),
        memory: pick("memory", &project.memory, &user.memory, text, u),
        trust: pick("trust", &project.trust, &user.trust, list, u),
        allow: pick("allow", &project.allow, &user.allow, list, u),
        mcp_server: pick("mcp_server", &project.mcp_server, &user.mcp_server, list, u),
        no_auto_mcp: pick(
            "no_auto_mcp",
            &project.no_auto_mcp,
            &user.no_auto_mcp,
            any,
            u,
        ),
        on_pretool: pick("on_pretool", &project.on_pretool, &user.on_pretool, list, u),
        on_posttool: pick(
            "on_posttool",
            &project.on_posttool,
            &user.on_posttool,
            list,
            u,
        ),
        on_turnend: pick("on_turnend", &project.on_turnend, &user.on_turnend, list, u),
        skill: pick("skill", &project.skill, &user.skill, list, u),
        no_skills: pick("no_skills", &project.no_skills, &user.no_skills, any, u),
        system_prompt: pick(
            "system_prompt",
            &project.system_prompt,
            &user.system_prompt,
            text,
            u,
        ),
        context_window: pick(
            "context_window",
            &project.context_window,
            &user.context_window,
            any,
            u,
        ),
        compact_at: pick("compact_at", &project.compact_at, &user.compact_at, any, u),
        no_compaction: pick(
            "no_compaction",
            &project.no_compaction,
            &user.no_compaction,
            any,
            u,
        ),
        reasoning_effort: pick(
            "reasoning_effort",
            &project.reasoning_effort,
            &user.reasoning_effort,
            text,
            u,
        ),
        // The one key where the project layer does NOT simply win: a cloned
        // repository must not be able to loosen the review gate the user
        // chose, so the STRICTER of the two layers applies (a project may
        // only keep or tighten it).
        review_gate: match (&project.review_gate, &user.review_gate) {
            (Some(p), Some(u)) => {
                if gate_strictness(u) > gate_strictness(p) {
                    u.clone().into()
                } else {
                    p.clone().into()
                }
            }
            (p, u) => p.clone().or_else(|| u.clone()),
        },
        mission: (project.mission.is_some() || user.mission.is_some()).then_some(mission),
        // The project layer NEVER supplies the theme, colors or keys (#463).
        tui: user.tui.clone(),
        // Nor the scheduler limits (#579).
        cron: user.cron.clone(),
        // Nor the delegation limits (#585).
        delegation: user.delegation.clone(),
        // Nor profiles (#584): a cloned repository must not define personas,
        // their allow lists or their models.
        profiles: user.profiles.clone(),
        // Nor webhook routes (#582): a repository must not open or widen
        // an endpoint.
        hooks: user.hooks.clone(),
        // Routing (#586): tiers and rules from the user only; a project can
        // only turn it off or narrow the allowed tiers.
        router: crate::routing::layered(&project.router, &user.router),
        // A project may only narrow the secret release (#598).
        secrets: layered_secrets(&project.secrets, &user.secrets),
        user_keys: BTreeSet::new(),
    };
    if let Some(c) = &user.cron {
        if c.daily_token_cap.is_some() {
            from_user.insert("cron.daily_token_cap");
        }
        if c.max_concurrent.is_some() {
            from_user.insert("cron.max_concurrent");
        }
        if c.timezone.is_some() {
            from_user.insert("cron.timezone");
        }
        if c.tz_source.is_some() {
            from_user.insert("cron.tz_source");
        }
    }
    if let Some(t) = &user.tui {
        if t.theme.as_deref().is_some_and(|v| !v.trim().is_empty()) {
            from_user.insert("tui.theme");
        }
        if t.colors.as_ref().is_some_and(|c| !c.is_empty()) {
            from_user.insert("tui.colors");
        }
        if t.keys.as_ref().is_some_and(|k| !k.is_empty()) {
            from_user.insert("tui.keys");
        }
    }
    if out.review_gate.is_some() && out.review_gate != project.review_gate {
        from_user.insert("review_gate");
    }
    out.user_keys = from_user;
    out
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
    lines.push(
        "note: precedence is flag > env > config > user config > default; a higher layer \
         replaces a whole list key, flags can only turn boolean keys on, and --no-config \
         ignores both files."
            .into(),
    );
    Ok(lines.join("\n"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    fn tmpdir() -> PathBuf {
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        let d = std::env::temp_dir().join(format!("tole-config-{}-{n}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn p() -> PathBuf {
        PathBuf::from("cfg.toml")
    }

    const FULL: &str = r#"
model = "m"
base_url = "http://x"
sessions_dir = "s"
workspace = "w"
plan_mode = true
memory = "uteke"
trust = ["internal", "none"]
allow = ["read_file"]
mcp_server = ["a=b c"]
no_auto_mcp = true
on_pretool = ["p"]
on_posttool = ["q"]
on_turnend = ["cargo check"]
skill = ["skills/x/SKILL.md"]
no_skills = false
system_prompt = "hi"
context_window = 131072
compact_at = 0.75
no_compaction = false

[mission]
max_steps = 10
max_minutes = 5
max_tokens = 1000
verify = "cargo test"
verify_timeout = 60
"#;

    #[test]
    fn full_file_round_trips() {
        let c = parse(FULL, &p()).unwrap();
        assert_eq!(c.model.as_deref(), Some("m"));
        assert_eq!(c.plan_mode, Some(true));
        assert_eq!(c.no_skills, Some(false));
        assert_eq!(c.trust.as_deref().unwrap(), ["internal", "none"]);
        assert_eq!(c.skill.as_deref().unwrap(), ["skills/x/SKILL.md"]);
        let m = c.mission.unwrap();
        assert_eq!(
            (m.max_steps, m.max_minutes, m.max_tokens, m.verify_timeout),
            (Some(10), Some(5), Some(1000), Some(60))
        );
        assert_eq!(m.verify.as_deref(), Some("cargo test"));
        let lines = render(&parse(FULL, &p()).unwrap());
        assert!(lines.contains(&"model = \"m\"  [config]".to_string()));
        assert!(lines.contains(&"  - \"internal\"".to_string()));
        assert!(lines.contains(&"mission.max_tokens = 1000  [config]".to_string()));
    }

    #[test]
    fn tui_table_parses_is_strict_and_only_the_user_layer_survives_layering() {
        let c = parse(
            "[tui]\ntheme = \"light\"\n[tui.colors]\ntool = \"blue bold\"\n",
            &p(),
        )
        .unwrap();
        let t = c.tui.as_ref().unwrap();
        assert_eq!(t.theme.as_deref(), Some("light"));
        assert_eq!(t.colors.as_ref().unwrap()["tool"], "blue bold");
        // Unknown keys and wrong types are loud, like any other key; a bad
        // VALUE is not (it is checked when the theme is built).
        assert!(parse("[tui]\nthemes = \"x\"\n", &p()).is_err());
        assert!(parse("[tui]\ntheme = 3\n", &p()).is_err());
        assert!(parse("[tui]\ntheme = \"neon\"\n", &p()).is_ok());
        // The project layer never reaches the merged config.
        let project = parse("[tui]\ntheme = \"mono\"\n", &p()).unwrap();
        let m = layered(&project, &Config::default());
        assert_eq!(m.tui, None);
        assert!(m.user_keys.is_empty());
        let m = layered(&project, &c);
        assert_eq!(m.tui, c.tui);
        assert!(m.user_keys.contains("tui.theme") && m.user_keys.contains("tui.colors"));
    }

    #[test]
    fn delegation_table_is_bounded_and_user_layer_only() {
        let user = parse(
            "[delegation]\nmax_depth = 3\nmax_live_processes = 2\ngoal_budget = 9\n",
            &p(),
        )
        .unwrap();
        let d = user.delegation.clone().unwrap();
        assert_eq!(
            (d.max_depth, d.max_live_processes, d.goal_budget),
            (Some(3), Some(2), Some(9))
        );
        for bad in [
            "max_depth = 4",
            "max_depth = 0",
            "max_live_processes = 0",
            "goal_steps = 0",
            "depth = 2",
        ] {
            assert!(
                parse(&format!("[delegation]\n{bad}\n"), &p()).is_err(),
                "{bad}"
            );
        }
        let project = parse("[delegation]\nmax_depth = 3\n", &p()).unwrap();
        assert_eq!(layered(&project, &Config::default()).delegation, None);
        assert_eq!(layered(&project, &user).delegation, user.delegation);
        let shown = render(&project).join("\n");
        assert!(
            shown.contains("delegation.max_depth = 3") && shown.contains("IGNORED"),
            "{shown}"
        );
    }

    /// The resource-guard keys: bounded, user layer only, shown by
    /// `config check`.
    #[test]
    fn delegation_resource_keys_are_bounded_and_user_layer_only() {
        let user = parse(
            "[delegation]\nmin_free_memory_mb = 2048\nchild_memory_mb = 256\n\
             max_load_per_cpu = 2.5\nmin_free_disk_mb = 4096\nmin_free_pids_percent = 20\n\
             spawn_interval_secs = 5\nmachine_max_live = 3\nchild_nice = 15\n",
            &p(),
        )
        .unwrap();
        let t = user
            .delegation
            .as_ref()
            .unwrap()
            .resource_thresholds()
            .unwrap();
        assert_eq!(
            (
                t.min_free_memory_mb,
                t.child_memory_mb,
                t.max_load_per_cpu,
                t.min_free_disk_mb,
                t.min_free_pids_percent,
                t.spawn_interval_secs,
                t.machine_max_live,
                t.child_nice
            ),
            (2048, 256, 2.5, 4096, 20, 5, 3, 15)
        );
        for bad in [
            "min_free_memory_mb = 0",
            "min_free_memory_mb = -1",
            "child_memory_mb = 0",
            "max_load_per_cpu = 0.0",
            "max_load_per_cpu = -2.0",
            "max_load_per_cpu = 1000.0",
            "min_free_disk_mb = 0",
            "min_free_pids_percent = 0",
            "min_free_pids_percent = 95",
            "spawn_interval_secs = 0",
            "machine_max_live = 0",
            "machine_max_live = 100000",
            "child_nice = 20",
            "child_nice = -5",
            "max_load_per_cpu = \"high\"",
        ] {
            assert!(
                parse(&format!("[delegation]\n{bad}\n"), &p()).is_err(),
                "{bad}"
            );
        }
        let project = parse("[delegation]\nmachine_max_live = 64\n", &p()).unwrap();
        assert_eq!(layered(&project, &Config::default()).delegation, None);
        let shown = render(&project).join("\n");
        assert!(
            shown.contains("delegation.machine_max_live = 64") && shown.contains("IGNORED"),
            "{shown}"
        );
        let shown = render(&user).join("\n");
        assert!(
            shown.contains("delegation.max_load_per_cpu = 2.5"),
            "{shown}"
        );
    }

    #[test]
    fn cron_table_is_strict_and_user_layer_only() {
        let user = parse("[cron]\ndaily_token_cap = 50000\n", &p()).unwrap();
        assert_eq!(user.cron.as_ref().unwrap().daily_token_cap, Some(50_000));
        assert!(parse("[cron]\ndaily_cap = 1\n", &p()).is_err());
        // Exempt from the secret-name rule only at that exact position.
        assert!(parse("[cron]\napi_token = \"x\"\n", &p()).is_err());
        assert!(parse("[mission]\ndaily_token_cap = 1\n", &p()).is_err());
        assert!(parse("[cron]\ndaily_token_cap = \"1\"\n", &p()).is_err());
        // #580: the zone keys; an unknown source names the accepted ones.
        let tz = parse(
            "[cron]\ntimezone = \"Asia/Jakarta\"\ntz_source = \"offset-only\"\n",
            &p(),
        )
        .unwrap()
        .cron
        .unwrap();
        assert_eq!(
            (tz.timezone.as_deref(), tz.tz_source),
            (Some("Asia/Jakarta"), Some(TzSource::OffsetOnly))
        );
        let e = parse("[cron]\ntz_source = \"bundled\"\n", &p())
            .unwrap_err()
            .to_string();
        assert!(e.contains("system") && e.contains("offset-only"), "{e}");
        let project = parse("[cron]\ndaily_token_cap = 999999999\n", &p()).unwrap();
        assert_eq!(layered(&project, &Config::default()).cron, None);
        let m = layered(&project, &user);
        assert_eq!(m.cron, user.cron);
        assert!(m.user_keys.contains("cron.daily_token_cap"));
        let shown = render(&project).join("\n");
        assert!(shown.contains("cron.daily_token_cap = 999999999") && shown.contains("IGNORED"));
    }

    /// #582: webhook routes come from the user config only. A project
    /// config can neither define a route nor change a user route, and
    /// `config check` shows route names, never where a key comes from.
    #[test]
    fn hooks_are_user_layer_only_and_shown_by_name() {
        let user = parse(
            "[hooks.gh]\nhmac_key_env = \"GH_HOOK_KEY\"\nrate_per_minute = 5\n",
            &p(),
        )
        .unwrap();
        let project = parse(
            "[hooks.gh]\ninsecure_loopback = true\nrate_per_minute = 600\n[hooks.evil]\nhmac_key_env = \"X\"\n",
            &p(),
        )
        .unwrap();
        assert_eq!(layered(&project, &Config::default()).hooks, None);
        assert_eq!(layered(&project, &user).hooks, user.hooks);
        assert!(parse("[hooks.gh]\nsecret = \"x\"\n", &p()).is_err());
        let shown = render(&user).join("\n");
        assert!(
            shown.contains("hooks = [1 entries]") && shown.contains("gh"),
            "{shown}"
        );
        assert!(!shown.contains("GH_HOOK_KEY"), "{shown}");
        assert!(render(&project).join("\n").contains("IGNORED"));
    }

    /// #579: `cron.max_concurrent` is a bounded, user-layer-only key.
    #[test]
    fn cron_max_concurrent_is_bounded_and_user_layer_only() {
        let user = parse("[cron]\nmax_concurrent = 8\n", &p()).unwrap();
        assert_eq!(user.cron.as_ref().unwrap().max_concurrent, Some(8));
        for bad in ["0", "65", "-1", "\"4\""] {
            assert!(
                parse(&format!("[cron]\nmax_concurrent = {bad}\n"), &p()).is_err(),
                "{bad}"
            );
        }
        let project = parse("[cron]\nmax_concurrent = 64\n", &p()).unwrap();
        assert_eq!(layered(&project, &Config::default()).cron, None);
        let m = layered(&project, &user);
        assert_eq!(m.cron, user.cron);
        assert!(m.user_keys.contains("cron.max_concurrent"));
        let shown = render(&project).join("\n");
        assert!(shown.contains("cron.max_concurrent = 64") && shown.contains("IGNORED"));
    }

    #[test]
    fn profiles_parse_strictly_and_only_the_user_layer_survives_layering() {
        let src = "[profiles.coder]\nallow = [\"write_*\"]\nmemory_room = \"profile-code\"\n\
                   grants = [\"GITHUB_TOKEN\"]\n[profiles.coder.models]\nstandard = \"m\"\n";
        let c = parse(src, &p()).unwrap();
        let coder = &c.profiles.as_ref().unwrap()["coder"];
        assert_eq!(coder.allow.as_deref(), Some(&["write_*".to_string()][..]));
        assert_eq!(coder.models.as_ref().unwrap().default_tier(), "standard");
        // Unknown keys, bad names, bad rooms and impossible defaults are loud.
        for (bad, want) in [
            ("[profiles.coder]\nmodel = \"m\"\n", "unknown field"),
            ("[profiles.Coder]\n", "profile name `Coder`"),
            ("[profiles.\"-x\"]\n", "profile name `-x`"),
            ("[profiles.a]\nmemory_room = \"a b\"\n", "memory_room"),
            (
                "[profiles.a.models]\nlight = \"s\"\n",
                "models.standard is not set",
            ),
            (
                "[profiles.a.models]\nadvisor = \"s\"\ndefault = \"advisor\"\n",
                "light, standard or heavy",
            ),
        ] {
            let e = parse(bad, &p()).unwrap_err().to_string();
            assert!(e.contains(want), "{bad:?}: {e}");
        }
        // A grant is a NAME; a value pasted by mistake is refused and never echoed.
        let e = parse("[profiles.a]\ngrants = [\"ghp-abcdef123456\"]\n", &p())
            .unwrap_err()
            .to_string();
        assert!(
            e.contains("environment variable NAME") && !e.contains("ghp-abcdef"),
            "{e}"
        );
        // The secret-key guard still applies inside a profile.
        assert!(parse("[profiles.a]\napi_key = \"x\"\n", &p()).is_err());
        // The project layer never supplies profiles.
        let m = layered(&c, &Config::default());
        assert_eq!(m.profiles, None);
        let m = layered(&Config::default(), &c);
        assert_eq!(m.profiles, c.profiles);
        let lines = render(&c);
        assert!(lines.iter().any(|l| l
            == "profiles = [1 entries]  [config]  IGNORED: a project config cannot define profiles (user config only)"));
        assert!(lines.contains(&"  - \"coder\"".to_string()));
    }

    #[test]
    fn tui_keys_parse_as_string_or_list_are_type_strict_and_user_layer_only() {
        let c = parse(
            "[tui.keys]\ntoggle_tools = \"ctrl+o\"\nnewline = [\"shift+enter\", \"alt+n\"]\n",
            &p(),
        )
        .unwrap();
        let k = c.tui.as_ref().unwrap().keys.as_ref().unwrap();
        assert_eq!(k["toggle_tools"].to_vec(), vec!["ctrl+o"]);
        assert_eq!(k["newline"].to_vec(), vec!["shift+enter", "alt+n"]);
        // Wrong types are loud; a bad action NAME or spec is not (it is
        // checked, with a note, when the keymap is built).
        assert!(parse("[tui.keys]\ntoggle_tools = 3\n", &p()).is_err());
        assert!(parse("[tui.keys]\ntoggle_tools = [1]\n", &p()).is_err());
        assert!(parse("[tui]\nkeys = \"x\"\n", &p()).is_err());
        assert!(parse("[tui.keys]\nno_such_action = \"ctrl+o\"\n", &p()).is_ok());
        // The project layer never reaches the merged config.
        let project = parse("[tui.keys]\ntoggle_tools = \"f5\"\n", &p()).unwrap();
        let m = layered(&project, &Config::default());
        assert_eq!(m.tui, None);
        assert!(m.user_keys.is_empty());
        let m = layered(&project, &c);
        assert_eq!(m.tui, c.tui);
        assert!(m.user_keys.contains("tui.keys"));
        // `config check` rendering lists the entries.
        let lines = render(&c);
        assert!(lines
            .iter()
            .any(|l| l.starts_with("tui.keys = [2 entries]  [config]")));
        assert!(lines.contains(&"  - newline = [\"shift+enter\", \"alt+n\"]".to_string()));
    }

    #[test]
    fn review_gate_is_validated_and_a_project_can_only_tighten_it() {
        assert_eq!(
            parse("review_gate = \"warn\"\n", &p())
                .unwrap()
                .review_gate
                .as_deref(),
            Some("warn")
        );
        let e = parse("review_gate = \"maybe\"\n", &p()).unwrap_err();
        assert!(e.message.contains("review_gate must be"), "{e}");
        let with = |v: &str| parse(&format!("review_gate = \"{v}\"\n"), &p()).unwrap();
        let eff = |proj: Option<&str>, user: Option<&str>| {
            let proj = proj.map(with).unwrap_or_default();
            let user = user.map(with).unwrap_or_default();
            let m = layered(&proj, &user);
            (m.review_gate.clone(), m.user_keys.contains("review_gate"))
        };
        let s = |v: &str| Some(v.to_string());
        // A project that loosens what the user chose loses to the user.
        assert_eq!(eff(Some("off"), Some("block")), (s("block"), true));
        assert_eq!(eff(Some("warn"), Some("block")), (s("block"), true));
        // A project that tightens wins; equal is the project's.
        assert_eq!(eff(Some("block"), Some("off")), (s("block"), false));
        assert_eq!(eff(Some("warn"), Some("warn")), (s("warn"), false));
        // One layer only: used as given (the project file is trust-gated).
        assert_eq!(eff(None, Some("off")), (s("off"), true));
        assert_eq!(eff(Some("off"), None), (s("off"), false));
        assert_eq!(eff(None, None), (None, false));
    }

    #[test]
    fn reasoning_effort_is_validated_and_layered() {
        let c = parse("reasoning_effort = \"high\"\n", &p()).unwrap();
        assert_eq!(c.reasoning_effort.as_deref(), Some("high"));
        let lines = render(&c);
        assert!(lines.contains(&"reasoning_effort = \"high\"  [config]".to_string()));
        let e = parse("reasoning_effort = \"max\"\n", &p()).unwrap_err();
        assert!(e.message.contains("minimal | low | medium | high"), "{e}");
        let user = parse("reasoning_effort = \"low\"\n", &p()).unwrap();
        let none = Config::default();
        let m = layered(&none, &user);
        assert_eq!(m.reasoning_effort.as_deref(), Some("low"));
        assert!(m.user_keys.contains("reasoning_effort"));
        assert_eq!(
            layered(&c, &user).reasoning_effort.as_deref(),
            Some("high"),
            "project wins key by key"
        );
    }

    #[test]
    fn compaction_keys_are_range_checked() {
        let c = parse(FULL, &p()).unwrap();
        assert_eq!(
            (c.context_window, c.compact_at, c.no_compaction),
            (Some(131072), Some(0.75), Some(false))
        );
        for bad in [
            "compact_at = 0.0\n",
            "compact_at = 1.5\n",
            "compact_at = -0.2\n",
        ] {
            let e = parse(bad, &p()).unwrap_err();
            assert!(e.message.contains("compact_at"), "{bad} -> {e}");
        }
        assert!(parse("compact_at = 1.0\n", &p()).is_ok());
        assert!(parse("context_window = 0\n", &p()).is_err());
        assert!(parse("context_window = -1\n", &p()).is_err());
    }

    #[test]
    fn empty_file_is_valid() {
        assert_eq!(parse("", &p()).unwrap(), Config::default());
        assert!(render(&Config::default()).is_empty());
    }

    #[test]
    fn unknown_key_is_rejected_naming_it() {
        let e = parse("modle = \"x\"\n", &p()).unwrap_err();
        assert!(e.message.contains("modle"), "{e}");
        assert_eq!(e.line_col.map(|l| l.0), Some(1));
        let e = parse("[mission]\nmax_step = 1\n", &p()).unwrap_err();
        assert!(e.message.contains("max_step"), "{e}");
    }

    #[test]
    fn forbidden_secret_keys_are_hard_errors() {
        for src in [
            "api_key = \"x\"\n",
            "serve_token = \"x\"\n",
            "token = \"x\"\n",
            "my_secret = \"x\"\n",
            "db_password = \"x\"\n",
            "API_KEY = \"x\"\n",
            "Api-Key = \"x\"\n",
            "[mission]\ntoken = \"x\"\n",
            "[mission]\nverify_secret = \"x\"\n",
            "[a.b]\nPassword = \"x\"\n",
            "x = [{ ok = 1, auth_token = \"x\" }]\n",
        ] {
            let e = parse(src, &p()).unwrap_err();
            assert!(
                e.message
                    .contains("secrets must never live in a config file"),
                "{src:?} -> {e}"
            );
            assert!(e.line_col.is_some(), "{src:?} -> {e}");
        }
        let e = parse("[mission]\nverify_secret = \"x\"\n", &p()).unwrap_err();
        assert!(e.message.contains("verify_secret"));
    }

    #[test]
    fn mission_max_tokens_is_not_mistaken_for_a_secret() {
        assert!(parse("[mission]\nmax_tokens = 5\n", &p()).is_ok());
        // Only in its exact position: top-level it is a secret-like name.
        assert!(parse("max_tokens = 5\n", &p()).is_err());
    }

    #[test]
    fn wrong_type_is_rejected_with_line_col() {
        let e = parse("model = \"m\"\nplan_mode = \"yes\"\n", &p()).unwrap_err();
        assert_eq!(e.line_col.map(|l| l.0), Some(2), "{e}");
        assert!(e.to_string().starts_with("cfg.toml:2:"), "{e}");
        assert!(parse("[mission]\nmax_steps = -1\n", &p()).is_err());
        assert!(parse("[mission]\nmax_steps = \"3\"\n", &p()).is_err());
        assert!(parse("allow = \"read_file\"\n", &p()).is_err());
    }

    #[test]
    fn trust_presets_use_the_shared_rule() {
        assert!(parse("trust = [\"internal\", \"read_only\", \"none\"]\n", &p()).is_ok());
        assert!(parse("trust = [\"INTERNAL\"]\n", &p()).is_ok());
        let e = parse("trust = [\"internal\", \"internalx\"]\n", &p()).unwrap_err();
        assert!(e.message.contains("unknown trust preset"), "{e}");
        assert!(e.message.contains("internalx"), "{e}");
        assert_eq!(e.line_col.map(|l| l.0), Some(1));
        // Same function as the flag: identical message.
        let flag = crate::trust::expand_trust(&["internalx".into()]).unwrap_err();
        assert_eq!(e.message, flag.to_string());
    }

    #[test]
    fn malformed_toml_reports_line_and_col() {
        let e = parse("model = \"m\"\nallow = [\n", &p()).unwrap_err();
        assert!(e.line_col.is_some(), "{e}");
        let e = parse("model = \"m\"\nbad line here\n", &p()).unwrap_err();
        let s = e.to_string();
        assert!(s.starts_with("cfg.toml:2:"), "{s}");
    }

    #[test]
    fn oversized_file_is_rejected_without_full_read() {
        let d = tmpdir();
        let f = d.join("big.toml");
        let big = format!("system_prompt = \"{}\"\n", "a".repeat(70 * 1024));
        std::fs::write(&f, big).unwrap();
        let e = load(&f).unwrap_err();
        assert!(e.message.contains("larger than the 64 KiB limit"), "{e}");
        // Exactly at the cap is allowed by the size gate.
        let ok = d.join("ok.toml");
        let prefix = "system_prompt = \"";
        let pad = MAX_CONFIG_BYTES as usize - prefix.len() - 2;
        std::fs::write(&ok, format!("{prefix}{}\"\n", "b".repeat(pad))).unwrap();
        assert_eq!(
            std::fs::metadata(&ok).unwrap().len(),
            MAX_CONFIG_BYTES,
            "fixture must sit exactly at the cap"
        );
        assert!(load(&ok).is_ok());
        // The bounded reader never pulls more than cap+1 bytes.
        let huge = d.join("huge.toml");
        let f = std::fs::File::create(&huge).unwrap();
        f.set_len(64 * 1024 * 1024).unwrap();
        let t = std::time::Instant::now();
        assert!(load(&huge).is_err());
        assert!(t.elapsed() < std::time::Duration::from_secs(5));
    }

    #[test]
    fn non_utf8_is_rejected() {
        let d = tmpdir();
        let f = d.join("bin.toml");
        std::fs::write(&f, [0xff, 0xfe, 0x00]).unwrap();
        assert!(load(&f).unwrap_err().message.contains("UTF-8"));
    }

    #[test]
    fn discovery_is_cwd_only_and_explicit_wins() {
        let d = tmpdir();
        assert_eq!(discover(&d, None), None);
        std::fs::create_dir_all(d.join(".tole")).unwrap();
        std::fs::write(d.join(".tole/config.toml"), "").unwrap();
        assert_eq!(discover(&d, None), Some(d.join(".tole/config.toml")));
        // No parent walk.
        let child = d.join("sub");
        std::fs::create_dir_all(&child).unwrap();
        assert_eq!(discover(&child, None), None);
        let other = d.join("other.toml");
        assert_eq!(discover(&d, Some(&other)), Some(other));
    }

    #[test]
    fn check_reports_absent_valid_and_missing_explicit() {
        let d = tmpdir();
        let none = |_: &Config| Annotations::new();
        let out = check(&d, None, None, &none).unwrap();
        assert!(out.contains("\nno .tole/config.toml in "), "{out}");
        let f = d.join("c.toml");
        std::fs::write(&f, "model = \"m\"\n").unwrap();
        let out = check(&d, Some(&f), None, &none).unwrap();
        assert!(out.contains("model = \"m\"  [config]"), "{out}");
        assert!(check(&d, Some(&d.join("nope.toml")), None, &none).is_err());
    }

    #[test]
    fn check_shows_both_layers() {
        let d = tmpdir();
        let none = |_: &Config| Annotations::new();
        let user = d.join("user.toml");
        let out = check(&d, None, Some(&user), &none).unwrap();
        assert!(out.starts_with("no user config at "), "{out}");
        std::fs::write(&user, "model = \"u\"\nskill = [\"g/SKILL.md\"]\n").unwrap();
        let proj = d.join("p.toml");
        std::fs::write(&proj, "model = \"p\"\n").unwrap();
        let out = check(&d, Some(&proj), Some(&user), &|c| {
            // The annotation sees the layered config.
            assert_eq!(c.model.as_deref(), Some("p"));
            assert!(c.user_keys.contains("skill") && !c.user_keys.contains("model"));
            Annotations::new()
        })
        .unwrap();
        assert!(out.starts_with("user config: "), "{out}");
        assert!(out.contains("model = \"u\"  [user config]"), "{out}");
        assert!(out.contains("model = \"p\"  [config]"), "{out}");
        // A broken user config is an error, like a broken project config.
        std::fs::write(&user, "api_key = \"x\"\n").unwrap();
        assert!(check(&d, None, Some(&user), &none).is_err());
    }

    #[test]
    fn project_layer_wins_key_by_key() {
        let user = parse(
            "model = \"u\"\nbase_url = \"http://u\"\nskill = [\"g\"]\nplan_mode = true\n\
             [mission]\nmax_steps = 9\nverify = \"u\"\n",
            &p(),
        )
        .unwrap();
        let project = parse(
            "model = \"p\"\nbase_url = \"  \"\nskill = []\nplan_mode = false\n\
             [mission]\nverify = \"p\"\n",
            &p(),
        )
        .unwrap();
        let c = layered(&project, &user);
        assert_eq!(c.model.as_deref(), Some("p"));
        // Blank string / empty list in the project count as unset.
        assert_eq!(c.base_url.as_deref(), Some("http://u"));
        assert_eq!(c.skill.as_deref().unwrap(), ["g"]);
        // An explicit project `false` is set: it wins over the user `true`.
        assert_eq!(c.plan_mode, Some(false));
        let m = c.mission.clone().unwrap();
        assert_eq!((m.max_steps, m.verify.as_deref()), (Some(9), Some("p")));
        let expected: BTreeSet<&str> = ["base_url", "skill", "mission.max_steps"].into();
        assert_eq!(c.user_keys, expected);
        // No layers at all: nothing set, no mission table.
        assert_eq!(
            layered(&Config::default(), &Config::default()),
            Config::default()
        );
        // One layer alone passes through.
        let only_user = layered(&Config::default(), &user);
        assert_eq!(only_user.model.as_deref(), Some("u"));
        assert!(only_user.user_keys.contains("model"));
    }

    #[test]
    fn long_system_prompt_is_truncated_for_display() {
        let c = parse(&format!("system_prompt = \"{}\"\n", "x".repeat(200)), &p()).unwrap();
        let line = &render(&c)[0];
        assert!(line.contains("… (200 chars)"), "{line}");
        assert!(line.len() < 150, "{line}");
    }

    #[test]
    fn secrets_bind_names_to_tools_and_a_project_can_only_narrow_them() {
        let p = |src: &str| parse(src, Path::new("c.toml"));
        let user = p(
            "[secrets]\nGITHUB_TOKEN = { tools = [\"gh\", \"git\", \"run_command\"] }\n\
                      NPM_TOKEN = { tools = [\"run_command\"] }\n",
        )
        .unwrap();
        // A key that is a secret NAME is not a forbidden key.
        let r = user.release();
        assert!(r.allows("GITHUB_TOKEN", "run_command"));
        assert!(r.allows("NPM_TOKEN", "run_command"));
        // A project tries to widen (job_start, a new secret) and narrows git.
        let project = p(
            "[secrets]\nGITHUB_TOKEN = { tools = [\"gh\", \"job_start\"] }\n\
                         AWS_SECRET_ACCESS_KEY = { tools = [\"run_command\"] }\n",
        )
        .unwrap();
        let l = layered(&project, &user).release();
        assert!(l.allows("GITHUB_TOKEN", "gh"));
        assert!(!l.allows("GITHUB_TOKEN", "job_start"), "widening refused");
        assert!(!l.allows("GITHUB_TOKEN", "run_command"), "narrowed");
        assert!(!l.allows("AWS_SECRET_ACCESS_KEY", "run_command"));
        let w = secrets_widening_warnings(&project, &user, "p").join("\n");
        assert!(w.contains("GITHUB_TOKEN to job_start"), "{w}");
        assert!(w.contains("AWS_SECRET_ACCESS_KEY to run_command"), "{w}");
        assert!(w.contains("in your user config"), "{w}");
        // Without a user entry, the default binding is the ceiling.
        let l = layered(&project, &Config::default()).release();
        assert!(l.allows("GITHUB_TOKEN", "gh"));
        assert!(!l.allows("GITHUB_TOKEN", "git"));
        assert!(!l.allows("GITHUB_TOKEN", "job_start"));
        // Bad entries name the remedy and never echo a key or value.
        for (src, needle) in [
            (
                "[secrets]\nPATH = { tools = [\"gh\"] }\n",
                "NAME of a secret",
            ),
            (
                "[secrets]\nghp_abcdefTOKEN = { tools = [\"gh\"] }\n",
                "NAME of a secret",
            ),
            (
                "[secrets]\nGITHUB_TOKEN = { tools = [\"Run Command\"] }\n",
                "tool name",
            ),
            (
                "[secrets]\nGITHUB_TOKEN = { tools = [\"gh\"], value = \"x\" }\n",
                "unknown field",
            ),
        ] {
            let e = p(src).unwrap_err().to_string();
            assert!(e.contains(needle), "{src}: {e}");
            assert!(!e.contains("ghp_abcdef"), "{e}");
        }
        // A secret-like key anywhere else is still forbidden.
        let e = p("[mission]\napi_token = \"x\"\n").unwrap_err().to_string();
        assert!(e.contains("forbidden key"), "{e}");
        // `config check` shows the binding, and on a project line the rule.
        let lines = render_with(&project, PROJECT_TAG, &Annotations::new()).join("\n");
        assert!(
            lines.contains("secrets.GITHUB_TOKEN = [\"gh\", \"job_start\"]"),
            "{lines}"
        );
        assert!(lines.contains("can only narrow"), "{lines}");
    }
}
