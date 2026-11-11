//! One scheduler pass (`tole cron tick`, #579).
//!
//! 1. Under the host lock (milliseconds): `recover` claims whose lease is
//!    dead, compute what is `ready`, record `missed`/`blocked` and `claim`
//!    each due occurrence (the claim is durable before anything runs). An
//!    agent run past `cron.daily_token_cap` is claimed and settled `budget`
//!    at once, without calling the provider.
//! 2. Without the lock: execute the claimed runs, at most
//!    `max_concurrent` at a time — script jobs as argv, agent jobs as one
//!    durable session each (see [`super::agent`]).
//! 3. Under the lock: `settle` every run. Then, without the lock, deliver
//!    it to each target ([`super::deliver`]) and record each delivery under
//!    the lock.
//!
//! A crash anywhere leaves either no claim (nothing ran) or a claim that
//! the next tick's `recover` turns into `interrupted` — never a re-run.
//! Run output is passed through the process's secret redactor (#595: env
//! secrets and web-header secrets) before it is written to disk.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use tole_core::cancel::CancelToken;
use tole_core::ledger::{Due, Fire, Job, Ledger, LedgerError, RunStatus};

use super::deliver::{self, RunInfo};
use super::{field, mode, store, DEFAULT_TIMEOUT_SECS};
use crate::approvals::ApprovalQueue;

/// Largest run output kept (bytes); the rest is cut with a marker.
const MAX_OUTPUT: usize = 1 << 20;

/// Tick tuning.
#[derive(Clone)]
pub struct TickOptions {
    pub max_concurrent: usize,
    /// `tole serve --cron`: the `/approvals` queue agent runs park
    /// non-allowlisted Writes in. `None` (standalone tick): refused.
    pub approvals: Option<Arc<ApprovalQueue>>,
    /// `cron.daily_token_cap` (user config): tokens all agent runs of one
    /// UTC day may spend.
    pub daily_token_cap: Option<u64>,
}

impl TickOptions {
    /// A standalone tick (no approvals queue, no daily cap).
    pub const fn standalone(max_concurrent: usize) -> Self {
        Self {
            max_concurrent,
            approvals: None,
            daily_token_cap: None,
        }
    }
}

/// What one tick did.
#[derive(Debug, Clone, Default)]
pub struct TickReport {
    pub recovered: usize,
    pub missed: u64,
    pub blocked: usize,
    /// `(fire_id, status)` of every run this tick settled.
    pub ran: Vec<(String, RunStatus)>,
}

fn safe_name(fire_id: &str) -> String {
    fire_id
        .chars()
        .map(|c| match c {
            'a'..='z' | 'A'..='Z' | '0'..='9' | '-' | '_' | '.' => c,
            _ => '_',
        })
        .collect()
}

/// Where a run's output is kept (`<dir>/runs/<fire id, path-safe>.out`).
pub fn output_path(dir: &Path, fire_id: &str) -> PathBuf {
    dir.join("runs").join(format!("{}.out", safe_name(fire_id)))
}

/// Start of the UTC day containing `t`.
pub fn day_start(t: i64) -> i64 {
    t - t.rem_euclid(86_400)
}

/// Tokens agent runs settled since `since` recorded.
pub fn tokens_since(state: &tole_core::ledger::LedgerState, since: i64) -> u64 {
    state
        .dones()
        .filter(|d| d.at >= since)
        .filter_map(|d| d.usage.as_ref()?.get("tokens")?.as_u64())
        .fold(0, u64::saturating_add)
}

/// A claimed occurrence, ready to execute.
pub struct Claimed {
    fire: Fire,
    job: Job,
    /// Token budget of this run (agent mode).
    max_tokens: u64,
}

/// What the claim phase knows (computed once per pass).
pub struct ClaimCtx {
    provider_ready: bool,
    cap: Option<u64>,
    used_today: u64,
}

impl ClaimCtx {
    pub fn new<S: tole_core::ledger::LedgerStore>(
        l: &Ledger<S>,
        now: i64,
        opts: &TickOptions,
    ) -> Self {
        Self {
            provider_ready: crate::turn_runner::provider_config().is_ok(),
            cap: opts.daily_token_cap,
            used_today: tokens_since(l.state(), day_start(now)),
        }
    }
}

/// Validates, then claims `due` (P10: a broken job is `blocked` and
/// spends nothing; past the daily cap an agent run settles `budget`).
pub fn claim_one<S: tole_core::ledger::LedgerStore>(
    l: &mut Ledger<S>,
    due: &Due,
    job: &Job,
    now: i64,
    ctx: &ClaimCtx,
    report: &mut TickReport,
) -> Result<Option<Claimed>, LedgerError> {
    let agent = mode(job) != "script";
    if due.fire.is_some() {
        let invalid = if agent && !ctx.provider_ready {
            Some((
                "provider_missing",
                crate::turn_runner::MISSING_PROVIDER_CONFIG.to_string(),
            ))
        } else {
            deliver::check(job).err()
        };
        if let Some((code, detail)) = invalid {
            l.block(due, code, Some(detail), now)?;
            report.blocked += 1;
            return Ok(None);
        }
    }
    let Some(fire) = l.claim(due, store::own_lease(), now)? else {
        return Ok(None);
    };
    let job_max = job
        .extra
        .get("max_tokens")
        .and_then(Value::as_u64)
        .unwrap_or(super::agent::DEFAULT_MAX_TOKENS);
    let left = ctx.cap.map(|c| c.saturating_sub(ctx.used_today));
    if agent && left == Some(0) {
        let usage = json!({"tokens": 0, "error": "daily token cap reached"});
        let done = l.settle(&fire.fire_id, RunStatus::Budget, None, Some(usage), now)?;
        report.ran.push((fire.fire_id, done.status));
        return Ok(None);
    }
    Ok(Some(Claimed {
        max_tokens: left.map_or(job_max, |left| left.min(job_max)),
        fire,
        job: job.clone(),
    }))
}

/// One finished run, ready to settle.
struct RunResult {
    status: RunStatus,
    session_id: Option<String>,
    output: String,
    usage: Value,
}

fn execute(c: &Claimed, opts: &TickOptions) -> RunResult {
    let started = Instant::now();
    if mode(&c.job) == "script" {
        let out = run_script(&c.job);
        RunResult {
            status: out.status,
            session_id: None,
            usage: json!({
                "tokens": 0,
                "exit_code": out.exit_code,
                "duration_ms": out.duration_ms,
                "error": out.error,
            }),
            output: out.stdout,
        }
    } else {
        let cancel = CancelToken::new();
        let run = super::agent::run_agent(&c.job, opts.approvals.as_ref(), &cancel, c.max_tokens);
        RunResult {
            status: run.status,
            usage: json!({
                "tokens": run.tokens,
                "max_tokens": c.max_tokens,
                "duration_ms": started.elapsed().as_millis(),
                "error": run.error,
                "approvals": if opts.approvals.is_some() { "parked" } else { "refused" },
            }),
            session_id: run.session_id,
            output: run.text,
        }
    }
}

/// One scheduler pass at `now` (Unix seconds).
pub fn tick(dir: &Path, now: i64, opts: &TickOptions) -> anyhow::Result<TickReport> {
    let mut report = TickReport::default();
    let claimed: Vec<Claimed> = store::with_lock(dir, |l| {
        report.recovered = l.recover(now, store::lease_alive)?.len();
        let ctx = ClaimCtx::new(l, now, opts);
        let mut claimed = Vec::new();
        for due in l.ready(now)? {
            report.missed += due.missed.as_ref().map_or(0, |m| m.count);
            let Some(job) = l.state().job(&due.job_id).map(|j| j.job.clone()) else {
                continue;
            };
            match claim_one(l, &due, &job, now, &ctx, &mut report) {
                Ok(Some(c)) => claimed.push(c),
                Ok(None) => {}
                // Another tick got there first: nothing to do here.
                Err(LedgerError::AlreadyClaimed { .. } | LedgerError::LostRace { .. }) => {}
                Err(LedgerError::AlreadyAccounted { .. } | LedgerError::JobInactive(_)) => {}
                Err(e) => return Err(e.into()),
            }
        }
        Ok(claimed)
    })?;
    run_claimed(dir, claimed, now, opts, &mut report)?;
    Ok(report)
}

/// Executes claimed runs (bounded), then settles and delivers each.
pub fn run_claimed(
    dir: &Path,
    claimed: Vec<Claimed>,
    now: i64,
    opts: &TickOptions,
    report: &mut TickReport,
) -> anyhow::Result<()> {
    let started = Instant::now();
    let results = run_bounded(claimed, opts.max_concurrent, |c| {
        let r = execute(&c, opts);
        (c, r)
    });
    let end = now + started.elapsed().as_secs() as i64;

    // Keep the output (redacted). A failed write is recorded on the run,
    // never a reason to leave a claim unsettled.
    let secrets = crate::provider_chain::process_secrets(&crate::config_apply::real_env);
    let results: Vec<(Claimed, RunResult)> = results
        .into_iter()
        .map(|(c, mut r)| {
            r.output = secrets.redact(&r.output).into_owned();
            let path = output_path(dir, &c.fire.fire_id);
            let written = std::fs::create_dir_all(dir.join("runs"))
                .and_then(|()| std::fs::write(&path, &r.output));
            if let (Err(e), Some(u)) = (written, r.usage.as_object_mut()) {
                u.insert("output_error".into(), e.to_string().into());
            }
            (c, r)
        })
        .collect();

    let mut first_err: Option<anyhow::Error> = None;
    let settled: Vec<(Claimed, RunResult)> = store::with_lock(dir, |l| {
        let mut settled = Vec::new();
        for (c, r) in results {
            let usage = Some(r.usage.clone());
            match l.settle(&c.fire.fire_id, r.status, r.session_id.clone(), usage, end) {
                Ok(d) => {
                    report.ran.push((c.fire.fire_id.clone(), d.status));
                    settled.push((c, r));
                }
                Err(e) => {
                    first_err.get_or_insert(e.into());
                }
            }
        }
        Ok(settled)
    })?;

    // Deliver without the lock (a command target may take seconds).
    let mut records = Vec::new();
    for (c, r) in &settled {
        let info = RunInfo {
            job: &c.job,
            fire_id: &c.fire.fire_id,
            scheduled_for: c.fire.scheduled_for,
            status: r.status,
            output: &r.output,
        };
        // Checked before the claim; an error here means a hand-edited log.
        for t in deliver::targets(&c.job).unwrap_or_default() {
            let (status, error) = deliver::deliver(&t, &info, &secrets);
            records.push((c.fire.fire_id.clone(), t.spec(), status, error));
        }
    }
    store::with_lock(dir, |l| {
        for (fire_id, target, status, error) in records {
            if let Err(e) = l.deliver(&fire_id, &target, status, error, end) {
                first_err.get_or_insert(e.into());
            }
        }
        Ok(())
    })?;
    match first_err {
        Some(e) => Err(e),
        None => Ok(()),
    }
}

/// The outcome of one script run.
#[derive(Debug, Clone)]
pub struct ScriptOutput {
    pub status: RunStatus,
    pub stdout: String,
    pub exit_code: Option<i32>,
    pub duration_ms: u128,
    pub error: Option<String>,
}

/// Runs a script job: argv (no shell), cwd = the job's workdir, the
/// secret-shaped environment scrubbed, a hard timeout. Zero tokens.
pub fn run_script(job: &Job) -> ScriptOutput {
    let start = Instant::now();
    let fail = |status: RunStatus, error: String| ScriptOutput {
        status,
        stdout: String::new(),
        exit_code: None,
        duration_ms: start.elapsed().as_millis(),
        error: Some(error),
    };
    let line = field(job, "script").unwrap_or_default();
    let argv = match tole_core::run_command::RunCommandTool::split_argv(line) {
        Ok(a) if !a.is_empty() => a,
        Ok(_) => return fail(RunStatus::Error, "empty script".into()),
        Err(e) => return fail(RunStatus::Error, e),
    };
    // Re-checked at run time: the log may have been edited by hand.
    if let Err(e) = tole_core::subprocess::check_destructive_argv(&argv) {
        return fail(RunStatus::Error, e);
    }
    let timeout = job
        .extra
        .get("timeout_secs")
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(DEFAULT_TIMEOUT_SECS);
    let mut cmd = Command::new(&argv[0]);
    cmd.args(&argv[1..]);
    if let Some(w) = field(job, "workdir") {
        cmd.current_dir(w);
    }
    tole_core::subprocess::scrub_env_for_child(&mut cmd);
    match tole_core::subprocess::run_with_timeout(&mut cmd, Duration::from_secs(timeout)) {
        Ok(out) => {
            let mut stdout = String::from_utf8_lossy(&out.stdout).into_owned();
            if stdout.len() > MAX_OUTPUT {
                let mut cut = MAX_OUTPUT;
                while !stdout.is_char_boundary(cut) {
                    cut -= 1;
                }
                stdout.truncate(cut);
                stdout.push_str("\n[output truncated]\n");
            }
            ScriptOutput {
                status: if out.status.success() {
                    RunStatus::Ok
                } else {
                    RunStatus::Error
                },
                stdout,
                exit_code: out.status.code(),
                duration_ms: start.elapsed().as_millis(),
                error: None,
            }
        }
        Err(e) => fail(RunStatus::Error, e),
    }
}

/// Runs `f` over `items` with at most `limit` in flight; results keep the
/// input order.
pub fn run_bounded<T: Send, R: Send>(
    items: Vec<T>,
    limit: usize,
    f: impl Fn(T) -> R + Sync,
) -> Vec<R> {
    let limit = limit.max(1);
    let mut out = Vec::with_capacity(items.len());
    let mut items = items.into_iter().peekable();
    while items.peek().is_some() {
        let batch: Vec<T> = items.by_ref().take(limit).collect();
        std::thread::scope(|s| {
            let handles: Vec<_> = batch.into_iter().map(|t| s.spawn(|| f(t))).collect();
            for h in handles {
                out.push(h.join().expect("cron run thread panicked"));
            }
        });
    }
    out
}
