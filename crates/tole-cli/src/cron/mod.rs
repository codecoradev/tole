//! `tole cron` (#579, epic #577): scheduled jobs on top of the append-only
//! ledger core (`tole_core::ledger`, #578). Design: `docs/automation-design.md`
//! §4–5, §8.
//!
//! The job store (`<dir>/jobs.jsonl`, see [`store`]), the
//! `create | edit | list | status | pause | resume | remove | run | cancel
//! | runs | tick` surface, the **script** mode (argv, no shell, zero LLM
//! tokens) and the **agent** mode (one durable session per run, see
//! [`agent`]: non-allowlisted Writes are parked in `/approvals` under
//! `tole serve --cron` and refused by a standalone tick; never
//! auto-approved), delivery targets ([`deliver`]), budgets (steps, minutes,
//! tokens, and the user config's `cron.daily_token_cap`) and ticker
//! liveness ([`status`]). A job that fails pre-dispatch validation (no
//! provider, a bad or untrusted delivery target, a skill that is gone, a
//! profile the user config no longer defines) is recorded as `blocked`
//! before it runs. Agent jobs take `--skill` and `--profile` ([`setup`]).
//! A tick runs at most `cron.max_concurrent` (user config, default 4) at
//! once.
//!
//! The cron dir is `$TOLE_CRON_DIR`, else `<codecora home>/tole/cron`.

pub mod agent;
pub mod deliver;
pub mod setup;
pub mod status;
pub use crate::ledger_file as store;
pub mod tick;

use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail, Context};
use clap::{Subcommand, ValueEnum};
use serde_json::{json, Map, Value};
use tole_core::cron::{civil_from_days, Schedule};
use tole_core::ledger::{CatchUp, Job, RunStatus};

pub use tick::{tick, TickOptions, TickReport};

/// Default per-run timeout of a script job, in seconds.
pub const DEFAULT_TIMEOUT_SECS: u64 = 300;
/// Runs one tick executes at once when `cron.max_concurrent` is unset.
pub const DEFAULT_MAX_CONCURRENT: usize = 4;

/// `tole cron <action>`. Parsed once per process, so the size difference
/// between `Create` and the other variants costs nothing worth boxing.
#[derive(Subcommand, Debug)]
#[allow(clippy::large_enum_variant)]
pub enum CronAction {
    /// Save a job. SCHEDULE is 5-field cron (`0 9 * * 1-5`), a macro
    /// (`@daily`) or `@every 30m`, in UTC. With --script the job runs that
    /// command (argv, no shell, zero tokens); otherwise PROMPT is an agent
    /// job: one session per run, Writes only via --allow (anything else is
    /// parked under `tole serve --cron`, refused by `tole cron tick`).
    /// Agent jobs also take the global --skill (repeatable: a skill name
    /// found from the workdir, or a SKILL.md file) and --profile NAME (a
    /// user-config profile: persona, model, skills, memory room; Writes then
    /// need both --allow and the profile's allow list). Both are validated
    /// on save and before every run (a run without them is `blocked`).
    Create {
        schedule: String,
        /// The agent prompt (agent mode) or a free-text note (script mode).
        prompt: Option<String>,
        /// Run this command line instead of an agent (no shell).
        #[arg(long)]
        script: Option<String>,
        /// Display name (default: the id).
        #[arg(long)]
        name: Option<String>,
        /// Explicit job id (default: generated).
        #[arg(long)]
        id: Option<String>,
        /// Working directory (absolute; default: the current directory).
        #[arg(long)]
        workdir: Option<PathBuf>,
        /// Write pre-authorization for agent runs (same syntax as `run --allow`).
        #[arg(long = "allow")]
        allow: Vec<String>,
        /// Model pin for agent runs.
        #[arg(long)]
        model: Option<String>,
        /// `skip` (default): only on-time runs; `one`: one late run within
        /// --grace-minutes.
        #[arg(long, value_enum, default_value_t = CatchUpArg::Skip)]
        catch_up: CatchUpArg,
        #[arg(long, default_value_t = 0)]
        grace_minutes: u32,
        /// Per-run timeout of a script job in seconds [default: 300].
        #[arg(long)]
        timeout: Option<u64>,
        /// Agent runs: tool-call budget [default: 32]; past it the run
        /// settles `budget`.
        #[arg(long)]
        max_steps: Option<u64>,
        /// Agent runs: wall-clock budget in minutes [default: 15, max: 10080].
        #[arg(long)]
        max_minutes: Option<u64>,
        /// Agent runs: token budget [default: 200000]; past it the run
        /// settles `budget`.
        #[arg(long)]
        max_tokens: Option<u64>,
        /// Delivery target, repeatable: `log`, `session`, `file:PATH`
        /// (inside the workdir) or `command:ARGV` (output on stdin)
        /// [default: session for agent jobs, log for scripts].
        #[arg(long = "deliver")]
        deliver: Vec<String>,
        /// Save the job paused.
        #[arg(long)]
        paused: bool,
    },
    /// Change a job; only the given fields change (lists are replaced).
    /// The mode stays: a script job keeps --script, an agent job its prompt.
    Edit {
        job: String,
        #[arg(long)]
        schedule: Option<String>,
        #[arg(long)]
        prompt: Option<String>,
        #[arg(long)]
        script: Option<String>,
        #[arg(long)]
        name: Option<String>,
        #[arg(long)]
        workdir: Option<PathBuf>,
        #[arg(long = "allow")]
        allow: Vec<String>,
        #[arg(long)]
        model: Option<String>,
        #[arg(long, value_enum)]
        catch_up: Option<CatchUpArg>,
        #[arg(long)]
        grace_minutes: Option<u32>,
        #[arg(long)]
        timeout: Option<u64>,
        #[arg(long)]
        max_steps: Option<u64>,
        #[arg(long)]
        max_minutes: Option<u64>,
        #[arg(long)]
        max_tokens: Option<u64>,
        #[arg(long = "deliver")]
        deliver: Vec<String>,
    },
    /// List jobs with their next run (UTC) and last outcome.
    List,
    /// Ticker liveness (last tick, host) and job, run and token counts.
    Status,
    /// Run a job now, in this process (approvals refused, as in `tick`).
    Run { job: String },
    /// Ask the job's running run to stop; it settles `cancelled`.
    Cancel { job: String },
    /// Pause a job (by id or name).
    Pause { job: String },
    /// Resume a paused job.
    Resume { job: String },
    /// Remove a job (a new record; history stays in the log).
    Remove { job: String },
    /// Show runs (newest last), optionally for one job.
    Runs { job: Option<String> },
    /// One scheduler pass: recover dead claims, record missed/blocked
    /// occurrences, run what is due (bounded concurrency), then exit.
    /// Host it from cron(8) or a systemd timer, e.g. every minute.
    Tick {
        /// [default: the user config's `cron.max_concurrent`, else 4]
        #[arg(long)]
        max_concurrent: Option<usize>,
    },
}

#[derive(Clone, Copy, Debug, ValueEnum)]
pub enum CatchUpArg {
    Skip,
    One,
}

/// The global `--skill` / `--profile` flags, which `cron create` and
/// `cron edit` record in the job (#579).
#[derive(Debug, Clone, Default)]
pub struct JobGlobals {
    pub skills: Vec<String>,
    pub profile: Option<String>,
}

/// What `create` saves.
#[derive(Debug, Clone, Default)]
pub struct CreateSpec {
    pub id: Option<String>,
    pub name: Option<String>,
    pub schedule: String,
    pub prompt: Option<String>,
    pub script: Option<String>,
    pub workdir: PathBuf,
    pub allow: Vec<String>,
    pub model: Option<String>,
    pub catch_up_one: bool,
    pub grace_minutes: u32,
    pub timeout_secs: Option<u64>,
    pub max_steps: Option<u64>,
    pub max_minutes: Option<u64>,
    pub max_tokens: Option<u64>,
    pub deliver: Vec<String>,
    pub skills: Vec<String>,
    pub profile: Option<String>,
    /// Pinned content hashes of `skills`, same order (`None`: pin now).
    pub skill_hashes: Option<Vec<String>>,
    /// Pinned hashes of the profile's skills (`None`: pin now).
    pub profile_skill_hashes: Option<std::collections::BTreeMap<String, String>>,
    pub paused: bool,
}

/// What `edit` changes (`None` / empty = keep).
#[derive(Debug, Clone, Default)]
pub struct EditSpec {
    pub schedule: Option<String>,
    pub prompt: Option<String>,
    pub script: Option<String>,
    pub name: Option<String>,
    pub workdir: Option<PathBuf>,
    pub allow: Vec<String>,
    pub model: Option<String>,
    pub catch_up_one: Option<bool>,
    pub grace_minutes: Option<u32>,
    pub timeout_secs: Option<u64>,
    pub max_steps: Option<u64>,
    pub max_minutes: Option<u64>,
    pub max_tokens: Option<u64>,
    pub deliver: Vec<String>,
    pub skills: Vec<String>,
    pub profile: Option<String>,
}

/// The cron dir: `$TOLE_CRON_DIR`, else `<codecora home>/tole/cron`.
pub fn default_dir() -> anyhow::Result<PathBuf> {
    if let Some(d) = std::env::var_os("TOLE_CRON_DIR").filter(|d| !d.is_empty()) {
        let d = PathBuf::from(d);
        if !d.is_absolute() {
            bail!("TOLE_CRON_DIR must be an absolute path");
        }
        return Ok(d);
    }
    tole_core::paths::tole_data_dir()
        .map(|d| d.join("cron"))
        .ok_or_else(|| anyhow!("cannot resolve the tole data dir (set CODECORA_HOME or HOME)"))
}

/// Current time in Unix seconds.
pub fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// `YYYY-MM-DD HH:MMZ` for Unix seconds.
pub fn fmt_utc(t: i64) -> String {
    let (y, m, d) = civil_from_days(t.div_euclid(86_400));
    let s = t.rem_euclid(86_400);
    format!(
        "{y:04}-{m:02}-{d:02} {:02}:{:02}Z",
        s / 3_600,
        s % 3_600 / 60
    )
}

/// A host-owned string field of a job.
pub fn field<'a>(job: &'a Job, key: &str) -> Option<&'a str> {
    job.extra.get(key).and_then(Value::as_str)
}

/// `script` or `agent`.
pub fn mode(job: &Job) -> &str {
    field(job, "mode").unwrap_or("agent")
}

/// The user config's `[cron]` table (`None` when unset or the user config
/// cannot be read; a broken file is reported).
fn cron_config() -> Option<crate::config::CronConfig> {
    match setup::user_config() {
        Ok(c) => c?.cron,
        Err(e) => {
            eprintln!("tole cron: user config not applied: {e}");
            None
        }
    }
}

/// The user config's `cron.daily_token_cap`.
pub fn daily_token_cap() -> Option<u64> {
    cron_config()?.daily_token_cap
}

/// Runs one tick executes at once: `flag`, else the user config's
/// `cron.max_concurrent`, else [`DEFAULT_MAX_CONCURRENT`].
pub fn max_concurrent(flag: Option<usize>) -> usize {
    flag.or_else(|| cron_config()?.max_concurrent)
        .unwrap_or(DEFAULT_MAX_CONCURRENT)
        .max(1)
}

/// Validates and saves a new job. Returns the saved record.
pub fn create(dir: &Path, spec: CreateSpec, now: i64) -> anyhow::Result<Job> {
    let id = match &spec.id {
        Some(id) => id.clone(),
        None => format!("j{now:x}{:04x}", std::process::id() & 0xffff),
    };
    let mut job = build_job(spec, id, now)?;
    job.extra.insert("created_by".into(), "cli".into());
    store::with_lock(dir, |l| {
        if l.state().job(&job.id).is_some_and(|j| !j.job.removed) {
            bail!("job `{}` already exists", job.id);
        }
        l.put_job(job.clone())?;
        Ok(())
    })?;
    Ok(job)
}

/// Validates a spec into a job record (without `created_by`).
fn build_job(spec: CreateSpec, id: String, now: i64) -> anyhow::Result<Job> {
    Schedule::parse(&spec.schedule)?;
    if !spec.workdir.is_absolute() || !spec.workdir.is_dir() {
        bail!(
            "workdir `{}` must be an existing absolute directory",
            spec.workdir.display()
        );
    }
    let mut extra = Map::new();
    match &spec.script {
        Some(line) => {
            let argv = tole_core::run_command::RunCommandTool::split_argv(line)
                .map_err(|e| anyhow!("--script: {e}"))?;
            if argv.is_empty() {
                bail!("--script is empty");
            }
            tole_core::subprocess::check_destructive_argv(&argv)
                .map_err(|e| anyhow!("--script: {e}"))?;
            extra.insert("mode".into(), "script".into());
            extra.insert("script".into(), line.clone().into());
        }
        None => {
            let p = spec.prompt.as_deref().unwrap_or("").trim();
            if p.is_empty() {
                bail!("an agent job needs a PROMPT (or use --script)");
            }
            extra.insert("mode".into(), "agent".into());
        }
    }
    if spec.script.is_some() && (!spec.skills.is_empty() || spec.profile.is_some()) {
        bail!("--skill and --profile apply to agent jobs only, not --script");
    }
    if !spec.skills.is_empty() {
        let (saved, hashes) = match spec.skill_hashes {
            // An edit that did not touch --skill keeps the owner's pins.
            Some(h) if h.len() == spec.skills.len() => (spec.skills.clone(), h),
            _ => setup::pin_skills(&spec.skills, &spec.workdir)
                .map_err(|e| anyhow!("--skill: {e}"))?,
        };
        extra.insert("skills".into(), json!(saved));
        extra.insert("skill_hashes".into(), json!(hashes));
    }
    if let Some(name) = spec.profile.as_deref().map(str::trim) {
        let profile = setup::resolve_profile(name).map_err(|e| anyhow!("--profile: {e}"))?;
        let pins = match spec.profile_skill_hashes {
            Some(p) => p,
            None => setup::pin_profile(&profile, &spec.workdir)
                .map_err(|e| anyhow!("--profile: {e}"))?,
        };
        extra.insert("profile".into(), name.into());
        if !pins.is_empty() {
            extra.insert("profile_skill_hashes".into(), json!(pins));
        }
    }
    if let Some(p) = &spec.prompt {
        extra.insert("prompt".into(), p.clone().into());
    }
    let mode = if spec.script.is_some() {
        "script"
    } else {
        "agent"
    };
    deliver::validate(mode, &spec.deliver).map_err(|e| anyhow!("--deliver: {e}"))?;
    if !spec.deliver.is_empty() {
        extra.insert("deliver".into(), json!(spec.deliver));
    }
    extra.insert(
        "name".into(),
        spec.name.unwrap_or_else(|| id.clone()).into(),
    );
    extra.insert("workdir".into(), spec.workdir.display().to_string().into());
    extra.insert(
        "timeout_secs".into(),
        spec.timeout_secs.unwrap_or(DEFAULT_TIMEOUT_SECS).into(),
    );
    if !spec.allow.is_empty() {
        extra.insert("allow".into(), json!(spec.allow));
    }
    if let Some(m) = spec.model {
        extra.insert("model".into(), m.into());
    }
    for (key, v) in [
        ("max_steps", spec.max_steps),
        ("max_minutes", spec.max_minutes),
        ("max_tokens", spec.max_tokens),
    ] {
        match v {
            Some(0) => bail!("--{} must be at least 1", key.replace('_', "-")),
            Some(n) if key == "max_minutes" && n > agent::MAX_MINUTES => {
                bail!(
                    "--max-minutes must be at most {} (one week)",
                    agent::MAX_MINUTES
                )
            }
            Some(n) => {
                extra.insert(key.into(), n.into());
            }
            None => {}
        }
    }
    Ok(Job {
        id,
        schedule: spec.schedule,
        timezone: None,
        catch_up: if spec.catch_up_one {
            CatchUp::One
        } else {
            CatchUp::Skip
        },
        grace_minutes: spec.grace_minutes,
        paused: spec.paused,
        removed: false,
        at: now,
        extra,
    })
}

/// The spec a saved job was built from.
fn spec_of(job: &Job) -> CreateSpec {
    let s = |k| field(job, k).map(str::to_string);
    let n = |k| job.extra.get(k).and_then(Value::as_u64);
    let list = |k| {
        job.extra
            .get(k)
            .and_then(|v| serde_json::from_value(v.clone()).ok())
            .unwrap_or_default()
    };
    CreateSpec {
        id: Some(job.id.clone()),
        name: s("name"),
        schedule: job.schedule.clone(),
        prompt: s("prompt"),
        script: s("script"),
        workdir: PathBuf::from(field(job, "workdir").unwrap_or_default()),
        allow: list("allow"),
        model: s("model"),
        catch_up_one: job.catch_up == CatchUp::One,
        grace_minutes: job.grace_minutes,
        timeout_secs: n("timeout_secs"),
        max_steps: n("max_steps"),
        max_minutes: n("max_minutes"),
        max_tokens: n("max_tokens"),
        deliver: list("deliver"),
        skills: list("skills"),
        profile: s("profile"),
        skill_hashes: Some(list("skill_hashes")),
        profile_skill_hashes: Some(
            job.extra
                .get("profile_skill_hashes")
                .and_then(|v| serde_json::from_value(v.clone()).ok())
                .unwrap_or_default(),
        ),
        paused: job.paused,
    }
}

/// Re-saves a job with `change` applied (validated like `create`; the
/// mode, pause state and `created_by` are kept).
pub fn edit(dir: &Path, key: &str, change: EditSpec, now: i64) -> anyhow::Result<Job> {
    store::with_lock(dir, |l| {
        let old = resolve(l.state(), key)?;
        let mut spec = spec_of(&old);
        if change.script.is_some() && spec.script.is_none() {
            bail!(
                "`{}` is an agent job; --script would change its mode (remove and re-create it)",
                old.id
            );
        }
        macro_rules! set {
            ($($f:ident),*) => { $( if change.$f.is_some() { spec.$f = change.$f; } )* };
        }
        set!(
            prompt,
            script,
            name,
            model,
            timeout_secs,
            max_steps,
            max_minutes,
            max_tokens
        );
        if let Some(sch) = change.schedule {
            spec.schedule = sch;
        }
        if let Some(w) = change.workdir {
            spec.workdir = w;
        }
        if let Some(c) = change.catch_up_one {
            spec.catch_up_one = c;
        }
        if let Some(g) = change.grace_minutes {
            spec.grace_minutes = g;
        }
        if !change.allow.is_empty() {
            spec.allow = change.allow;
        }
        if !change.deliver.is_empty() {
            spec.deliver = change.deliver;
        }
        // `--skill` / `--profile` re-pin (the owner chose the content now);
        // any other edit keeps the pins taken when they were last chosen.
        if !change.skills.is_empty() {
            spec.skills = change.skills;
            spec.skill_hashes = None;
        }
        if change.profile.is_some() {
            spec.profile = change.profile;
            spec.profile_skill_hashes = None;
        }
        let mut job = build_job(spec, old.id.clone(), now)?;
        if let Some(by) = old.extra.get("created_by") {
            job.extra.insert("created_by".into(), by.clone());
        }
        l.put_job(job.clone())?;
        Ok(job)
    })
}

/// Runs a job now in this process. Owed occurrences are settled first:
/// an on-time one is the run; a late one is recorded `missed` and the run
/// is claimed at `now`. Refused while the job is paused or running.
pub fn run_now(dir: &Path, key: &str, now: i64, opts: &TickOptions) -> anyhow::Result<TickReport> {
    let mut report = TickReport::default();
    let claimed = store::with_lock(dir, |l| {
        let job = resolve(l.state(), key)?;
        if job.paused {
            bail!("`{}` is paused; resume it first", job.id);
        }
        if l.state().pending().any(|f| f.job_id == job.id) {
            bail!("`{}` is already running", job.id);
        }
        let due = match l.ready(now)?.into_iter().find(|d| d.job_id == job.id) {
            Some(d) if d.fire.is_some() => d,
            Some(d) => tole_core::ledger::Due {
                fire: Some(now),
                ..d
            },
            None => tole_core::ledger::Due {
                job_id: job.id.clone(),
                fire: Some(now),
                missed: None,
            },
        };
        report.missed += due.missed.as_ref().map_or(0, |m| m.count);
        let ctx = tick::ClaimCtx::new(l, now, opts);
        Ok(tick::claim_one(l, &due, &job, now, &ctx, &mut report)?)
    })?;
    tick::run_claimed(dir, claimed.into_iter().collect(), now, opts, &mut report)?;
    Ok(report)
}

/// Asks the job's running run(s) to stop. Returns the fire ids.
pub fn cancel(dir: &Path, key: &str) -> anyhow::Result<Vec<String>> {
    let l = store::open_read(dir)?;
    let job = resolve(l.state(), key)?;
    let fires: Vec<String> = l
        .state()
        .pending()
        .filter(|f| f.job_id == job.id)
        .map(|f| f.fire_id.clone())
        .collect();
    if fires.is_empty() {
        bail!("`{}` has no running run", job.id);
    }
    std::fs::create_dir_all(dir.join("runs"))?;
    for f in &fires {
        std::fs::write(tick::cancel_path(dir, f), b"")?;
    }
    Ok(fires)
}

/// Resolves a job by id, else by case-insensitive name (ambiguous names
/// are refused with the candidate ids).
pub fn resolve(state: &tole_core::ledger::LedgerState, key: &str) -> anyhow::Result<Job> {
    let live = || state.jobs().filter(|j| !j.job.removed);
    if let Some(j) = live().find(|j| j.job.id == key) {
        return Ok(j.job.clone());
    }
    let hits: Vec<_> = live()
        .filter(|j| field(&j.job, "name").is_some_and(|n| n.eq_ignore_ascii_case(key)))
        .collect();
    match hits.as_slice() {
        [one] => Ok(one.job.clone()),
        [] => bail!("no job `{key}`"),
        many => bail!(
            "name `{key}` is ambiguous; use an id: {}",
            many.iter()
                .map(|j| j.job.id.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        ),
    }
}

/// Pause, resume or remove: a new job record (never a deletion).
pub fn update(
    dir: &Path,
    key: &str,
    now: i64,
    change: impl FnOnce(&mut Job),
) -> anyhow::Result<Job> {
    store::with_lock(dir, |l| {
        let mut job = resolve(l.state(), key)?;
        change(&mut job);
        job.at = now;
        l.put_job(job.clone())?;
        Ok(job)
    })
}

/// One `list` row.
#[derive(Debug, Clone)]
pub struct JobRow {
    pub id: String,
    pub name: String,
    pub schedule: String,
    pub mode: String,
    pub state: &'static str,
    pub next_run: Option<i64>,
    pub last: Option<RunStatus>,
    pub missed: u64,
    pub blocked: usize,
}

pub fn list(dir: &Path) -> anyhow::Result<Vec<JobRow>> {
    let l = store::open_read(dir)?;
    let s = l.state();
    Ok(s.jobs()
        .filter(|j| !j.job.removed)
        .map(|j| {
            let job = &j.job;
            JobRow {
                id: job.id.clone(),
                name: field(job, "name").unwrap_or(&job.id).to_string(),
                schedule: job.schedule.clone(),
                mode: mode(job).to_string(),
                state: if job.paused { "paused" } else { "active" },
                next_run: s.next_run(&job.id),
                last: s.last_outcome(&job.id).map(|d| d.status),
                missed: s
                    .missed()
                    .filter(|m| m.job_id == job.id)
                    .map(|m| m.count)
                    .sum(),
                blocked: s.blocked().filter(|b| b.job_id == job.id).count(),
            }
        })
        .collect())
}

/// One `runs` row.
#[derive(Debug, Clone)]
pub struct RunRow {
    pub fire_id: String,
    pub job_id: String,
    pub scheduled_for: i64,
    pub status: Option<RunStatus>,
    pub delivery: Option<String>,
    pub session_id: Option<String>,
    pub usage: Option<Value>,
    /// Every delivery attempt: `{target, attempt, status, error?}`.
    pub deliveries: Vec<Value>,
}

pub fn runs(dir: &Path, job: Option<&str>) -> anyhow::Result<Vec<RunRow>> {
    let l = store::open_read(dir)?;
    let s = l.state();
    let only = job.map(|k| resolve(s, k)).transpose()?.map(|j| j.id);
    let text = std::fs::read_to_string(store::ledger_path(dir)).unwrap_or_default();
    let mut rows = Vec::new();
    for line in text.lines() {
        let Ok(Some(tole_core::ledger::Record::Fire(f))) =
            tole_core::ledger::Record::parse_line(line)
        else {
            continue;
        };
        if only.as_ref().is_some_and(|j| *j != f.job_id)
            || s.claim_of(&f.job_id, f.scheduled_for) != Some(&f.fire_id)
        {
            continue;
        }
        let done = s.done(&f.fire_id);
        rows.push(RunRow {
            status: done.map(|d| d.status),
            session_id: done.and_then(|d| d.session_id.clone()),
            usage: done.and_then(|d| d.usage.clone()),
            deliveries: s
                .deliveries(&f.fire_id)
                .map(|d| serde_json::to_value(d).unwrap_or_default())
                .collect(),
            delivery: s
                .deliveries(&f.fire_id)
                .last()
                .map(|d| format!("{}:{:?}", d.target, d.status).to_lowercase()),
            fire_id: f.fire_id,
            job_id: f.job_id,
            scheduled_for: f.scheduled_for,
        });
    }
    Ok(rows)
}

/// Largest output `GET /cron/runs` returns per run (bytes).
pub const RUNS_OUTPUT_CAP: usize = 16 * 1024;

/// `GET /cron/runs[?job=J&limit=N]`: the newest `limit` runs (default 50,
/// at most 500), newest first, each with its (already redacted) output
/// capped at [`RUNS_OUTPUT_CAP`].
pub fn runs_json(dir: &Path, job: Option<&str>, limit: Option<usize>) -> anyhow::Result<Value> {
    let rows = runs(dir, job)?;
    let limit = limit.unwrap_or(50).clamp(1, 500);
    let list: Vec<Value> = rows
        .iter()
        .rev()
        .take(limit)
        .map(|r| {
            let mut out = std::fs::read(tick::output_path(dir, &r.fire_id)).unwrap_or_default();
            let truncated = out.len() > RUNS_OUTPUT_CAP;
            out.truncate(RUNS_OUTPUT_CAP);
            json!({
                "fire_id": r.fire_id,
                "job_id": r.job_id,
                "scheduled_for": r.scheduled_for,
                "status": r.status,
                "session_id": r.session_id,
                "usage": r.usage,
                "deliveries": r.deliveries,
                "output": String::from_utf8_lossy(&out),
                "output_truncated": truncated,
            })
        })
        .collect();
    Ok(json!({ "runs": list }))
}

fn status_word(s: Option<RunStatus>, none: &str) -> String {
    s.map(|s| format!("{s:?}").to_lowercase())
        .unwrap_or_else(|| none.into())
}

fn print_report(verb: &str, r: &TickReport) {
    println!(
        "{verb}: ran {}, recovered {}, missed {}, blocked {}",
        r.ran.len(),
        r.recovered,
        r.missed,
        r.blocked
    );
    for (fire, st) in &r.ran {
        println!("  {fire}  {}", status_word(Some(*st), "-"));
    }
}

/// Dispatch for `tole cron <action>` (prints to stdout). `globals` are the
/// global `--skill` / `--profile` flags; only `create` and `edit` take them.
pub fn cli(action: CronAction, globals: JobGlobals) -> anyhow::Result<()> {
    let dir = default_dir()?;
    let t = now();
    let takes_globals = matches!(action, CronAction::Create { .. } | CronAction::Edit { .. });
    if !takes_globals && (!globals.skills.is_empty() || globals.profile.is_some()) {
        bail!("--skill and --profile apply to `cron create` and `cron edit` only");
    }
    let JobGlobals {
        skills: skill,
        profile,
    } = globals;
    match action {
        CronAction::Create {
            schedule,
            prompt,
            script,
            name,
            id,
            workdir,
            allow,
            model,
            catch_up,
            grace_minutes,
            timeout,
            max_steps,
            max_minutes,
            max_tokens,
            deliver,
            paused,
        } => {
            let workdir = match workdir {
                Some(w) => w,
                None => std::env::current_dir().context("current dir")?,
            };
            let job = create(
                &dir,
                CreateSpec {
                    id,
                    name,
                    schedule,
                    prompt,
                    script,
                    workdir,
                    allow,
                    model,
                    catch_up_one: matches!(catch_up, CatchUpArg::One),
                    grace_minutes,
                    timeout_secs: timeout,
                    max_steps,
                    max_minutes,
                    max_tokens,
                    deliver,
                    skills: skill,
                    profile,
                    skill_hashes: None,
                    profile_skill_hashes: None,
                    paused,
                },
                t,
            )?;
            let next = store::open_read(&dir)?.state().next_run(&job.id);
            println!(
                "created {} ({}), next run: {}",
                job.id,
                mode(&job),
                next.map(fmt_utc).unwrap_or_else(|| "paused".into())
            );
        }
        CronAction::List => {
            let rows = list(&dir)?;
            if rows.is_empty() {
                println!("no jobs");
            }
            for r in rows {
                println!(
                    "{}  {}  [{}] {} {}  next: {}  last: {}  missed: {}  blocked: {}",
                    r.id,
                    r.name,
                    r.mode,
                    r.schedule,
                    r.state,
                    r.next_run.map(fmt_utc).unwrap_or_else(|| "-".into()),
                    r.last
                        .map(|s| format!("{s:?}").to_lowercase())
                        .unwrap_or_else(|| "-".into()),
                    r.missed,
                    r.blocked
                );
            }
        }
        CronAction::Pause { job } => {
            let j = update(&dir, &job, t, |j| j.paused = true)?;
            println!("paused {}", j.id);
        }
        CronAction::Resume { job } => {
            let j = update(&dir, &job, t, |j| j.paused = false)?;
            println!("resumed {}", j.id);
        }
        CronAction::Remove { job } => {
            let j = update(&dir, &job, t, |j| j.removed = true)?;
            println!("removed {}", j.id);
        }
        CronAction::Runs { job } => {
            for r in runs(&dir, job.as_deref())? {
                println!(
                    "{}  {}  {}  {}  {}",
                    r.fire_id,
                    r.job_id,
                    fmt_utc(r.scheduled_for),
                    status_word(r.status, "running"),
                    r.delivery.unwrap_or_else(|| "-".into())
                );
            }
        }
        CronAction::Edit {
            job,
            schedule,
            prompt,
            script,
            name,
            workdir,
            allow,
            model,
            catch_up,
            grace_minutes,
            timeout,
            max_steps,
            max_minutes,
            max_tokens,
            deliver,
        } => {
            let change = EditSpec {
                schedule,
                prompt,
                script,
                name,
                workdir,
                allow,
                model,
                catch_up_one: catch_up.map(|c| matches!(c, CatchUpArg::One)),
                grace_minutes,
                timeout_secs: timeout,
                max_steps,
                max_minutes,
                max_tokens,
                deliver,
                skills: skill,
                profile,
            };
            let j = edit(&dir, &job, change, t)?;
            let next = store::open_read(&dir)?.state().next_run(&j.id);
            println!(
                "edited {}, next run: {}",
                j.id,
                next.map(fmt_utc).unwrap_or_else(|| "paused".into())
            );
        }
        CronAction::Status => {
            let st = status::status(&dir, t)?;
            println!("cron dir: {}", dir.display());
            println!("{}", status::ticker_line(&st));
            println!(
                "jobs: {} active, {} paused; running {}; blocked {}; missed {}",
                st.active, st.paused, st.running, st.blocked, st.missed
            );
            let cap = daily_token_cap()
                .map(|c| c.to_string())
                .unwrap_or_else(|| "none".into());
            println!("tokens today (UTC): {} (daily cap: {cap})", st.tokens_today);
            println!("runs per tick: at most {}", max_concurrent(None));
        }
        CronAction::Run { job } => {
            let opts = TickOptions {
                daily_token_cap: daily_token_cap(),
                ..TickOptions::standalone(1)
            };
            print_report("run", &run_now(&dir, &job, t, &opts)?);
        }
        CronAction::Cancel { job } => {
            for f in cancel(&dir, &job)? {
                println!("cancel requested: {f}");
            }
        }
        CronAction::Tick {
            max_concurrent: flag,
        } => {
            let opts = TickOptions {
                daily_token_cap: daily_token_cap(),
                ..TickOptions::standalone(max_concurrent(flag))
            };
            print_report("tick", &tick(&dir, t, &opts)?);
        }
    }
    Ok(())
}

/// The `tole serve --cron` ticker: one pass per minute, just after the
/// minute boundary (cron occurrences are minute-aligned; jobs saved by
/// another process are seen at the next pass). Each pass runs on its own
/// thread, so a long run never delays the next minute's jobs (a job whose
/// run is still going is not due again). Agent runs park non-allowlisted
/// Writes in `approvals`. Never returns; a failed pass is logged.
pub fn serve_ticker(dir: &Path, approvals: std::sync::Arc<crate::approvals::ApprovalQueue>) {
    loop {
        let opts = TickOptions {
            max_concurrent: max_concurrent(None),
            approvals: Some(std::sync::Arc::clone(&approvals)),
            daily_token_cap: daily_token_cap(),
        };
        let dir = dir.to_path_buf();
        std::thread::spawn(move || pass(&dir, &opts));
        let wait = 60 - now().rem_euclid(60) + 1;
        std::thread::sleep(std::time::Duration::from_secs(wait as u64));
    }
}

fn pass(dir: &Path, opts: &TickOptions) {
    match tick(dir, now(), opts) {
        Ok(r) if !r.ran.is_empty() || r.recovered > 0 || r.blocked > 0 || r.missed > 0 => {
            eprintln!(
                "tole serve: cron tick ran {}, recovered {}, missed {}, blocked {}",
                r.ran.len(),
                r.recovered,
                r.missed,
                r.blocked
            );
        }
        Ok(_) => {}
        Err(e) => eprintln!("tole serve: cron tick failed: {e:#}"),
    }
}
