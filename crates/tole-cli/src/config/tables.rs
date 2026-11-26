//! The plain `[table]` schemas of the config file: mission, tui, cron, discussions,
//! provider, curator, hooks, webhook_out, delegation and router.

use serde::Deserialize;

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

/// The `[discussions]` table (#786). USER LAYER ONLY, like [`CronConfig`]:
/// a cloned repository must not stretch how long a provider call may hold
/// a discussion (and its budget) open.
#[derive(Debug, Default, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DiscussionsConfig {
    /// Seconds one discussion provider call may take in total (connect,
    /// send and read of the whole reply), 1 to 3600 (default 120).
    pub request_timeout_secs: Option<u64>,
}

/// The `[provider]` table (#1236). USER LAYER ONLY, like [`DiscussionsConfig`]:
/// a cloned repository must not set network-level timeouts of the owner's
/// provider.
#[derive(Debug, Default, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderConfig {
    /// Seconds one chat or mission provider request may take in total
    /// (connect, send and read of the whole streamed reply), 30 to 3600
    /// (default 900).
    pub request_timeout_secs: Option<u64>,
}

/// The `[curator]` table (#593). USER LAYER ONLY, like [`CronConfig`]: the
/// skill library is the user's, so a cloned repository's config must not
/// switch the curator off or shorten how long a skill lives.
#[derive(Debug, Default, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CuratorConfig {
    /// `false` turns `tole curator run` (and the weekly job) into a no-op
    /// that says so. Unset: on, the owner's default.
    pub enabled: Option<bool>,
    /// Days without use before an agent-grown skill is stale [default 14].
    pub stale_days: Option<u64>,
    /// Days without use before it is archived [default 30].
    pub archive_days: Option<u64>,
    /// Days a skill that was never used is left alone [default 30].
    pub grace_days: Option<u64>,
}

/// The curator's thresholds with the defaults filled in.
pub fn curator_thresholds(c: Option<&CuratorConfig>) -> tole_core::skill_curator::Thresholds {
    use tole_core::skill_curator::Thresholds;
    let d = Thresholds::default();
    Thresholds {
        stale_days: c.and_then(|c| c.stale_days).unwrap_or(d.stale_days),
        archive_days: c.and_then(|c| c.archive_days).unwrap_or(d.archive_days),
        grace_days: c.and_then(|c| c.grace_days).unwrap_or(d.grace_days),
    }
}

/// Upper bound of the curator's day counts (ten years).
pub const MAX_CURATOR_DAYS: u64 = 3650;

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

/// One `[webhook_out.<name>]` table (#911): a destination `tole` may push
/// signed events to (cron results, approval requests). USER LAYER ONLY: a
/// cloned repository must not open or redirect an outbound path, and no job,
/// skill or agent text can name a URL, a header or a secret: a cron job only
/// refers to a destination BY NAME (`deliver = ["webhook:<name>"]`). The
/// secret never lives here, only the name of the environment variable or the
/// path of the file that holds it. Design: `docs/webhook-delivery-design.md`.
#[derive(Debug, Default, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WebhookOutConfig {
    /// `https://...`; `http://` only for a loopback host with
    /// `allow_loopback`.
    pub url: Option<String>,
    /// Environment variable holding the HMAC secret.
    pub hmac_key_env: Option<String>,
    /// File holding the HMAC secret (a regular file, not a link, at most
    /// 4 KiB).
    pub hmac_key_file: Option<String>,
    /// Events sent (default: `cron_run_finished`). Approval events are
    /// opt-in.
    pub events: Option<Vec<String>>,
    /// Reach a loopback host (`localhost`, a loopback IP), and use `http`
    /// for it (default false).
    pub allow_loopback: Option<bool>,
    /// Reach private and link-local ranges (default false).
    pub allow_private: Option<bool>,
    /// Let a job created by an agent name this destination (default false).
    pub allow_agent_jobs: Option<bool>,
    /// Whole-request time in seconds (default 10, 1 to 30).
    pub timeout_secs: Option<u64>,
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
    /// Escalating approval queue for delegated children (#714): `off`
    /// (default; a child's Write stays denied), `owner` or `chain`.
    pub approvals: Option<tole_core::delegation_approvals::ApprovalsMode>,
    /// Seconds a request waits in all, 1 to 3600 (default 900).
    pub approval_secs: Option<u64>,
    /// Seconds each agent level gets before it moves up (default 120).
    pub approval_level_secs: Option<u64>,
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
