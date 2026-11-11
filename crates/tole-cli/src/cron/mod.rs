//! `tole cron` (#579, epic #577): scheduled jobs on top of the append-only
//! ledger core (`tole_core::ledger`, #578). Design: `docs/automation-design.md`
//! §4–5, §8.
//!
//! The job store (`<dir>/jobs.jsonl`, see [`store`]), the
//! `create | list | pause | resume | remove | runs | tick` surface, the
//! **script** mode (argv, no shell, zero LLM tokens) and the **agent** mode
//! (one durable session per run, see [`agent`]: non-allowlisted Writes are
//! parked in `/approvals` under `tole serve --cron` and refused by a
//! standalone tick; never auto-approved), delivery targets ([`deliver`])
//! and budgets (steps, minutes, tokens, and the user config's
//! `cron.daily_token_cap`). A job that fails pre-dispatch validation (no
//! provider, a bad or untrusted delivery target) is recorded as `blocked`
//! before it runs.
//!
//! The cron dir is `$TOLE_CRON_DIR`, else `<codecora home>/tole/cron`.

pub mod agent;
pub mod deliver;
pub mod store;
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
    /// List jobs with their next run (UTC) and last outcome.
    List,
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
        #[arg(long, default_value_t = 4)]
        max_concurrent: usize,
    },
}

#[derive(Clone, Copy, Debug, ValueEnum)]
pub enum CatchUpArg {
    Skip,
    One,
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
    pub paused: bool,
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

/// The user config's `cron.daily_token_cap` (`None` when unset or the
/// user config cannot be read; a broken file is reported).
pub fn daily_token_cap() -> Option<u64> {
    let path = crate::config::user_config_path().filter(|p| p.exists())?;
    match crate::config::load(&path) {
        Ok(c) => c.cron?.daily_token_cap,
        Err(e) => {
            eprintln!("tole cron: user config not applied: {e}");
            None
        }
    }
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
        rows.push(RunRow {
            status: s.done(&f.fire_id).map(|d| d.status),
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

/// Dispatch for `tole cron <action>` (prints to stdout).
pub fn cli(action: CronAction) -> anyhow::Result<()> {
    let dir = default_dir()?;
    let t = now();
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
        CronAction::Tick { max_concurrent } => {
            let opts = TickOptions {
                daily_token_cap: daily_token_cap(),
                ..TickOptions::standalone(max_concurrent)
            };
            print_report("tick", &tick(&dir, t, &opts)?);
        }
    }
    Ok(())
}

/// The `tole serve --cron` ticker: one tick per minute, just after the
/// minute boundary (cron occurrences are minute-aligned; jobs saved by
/// another process are seen at the next pass). Agent runs park
/// non-allowlisted Writes in `approvals`. Never returns; a failed pass is
/// logged and retried at the next minute.
pub fn serve_ticker(dir: &Path, approvals: std::sync::Arc<crate::approvals::ApprovalQueue>) {
    loop {
        let opts = TickOptions {
            max_concurrent: 4,
            approvals: Some(std::sync::Arc::clone(&approvals)),
            daily_token_cap: daily_token_cap(),
        };
        match tick(dir, now(), &opts) {
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
        let wait = 60 - now().rem_euclid(60) + 1;
        std::thread::sleep(std::time::Duration::from_secs(wait as u64));
    }
}
