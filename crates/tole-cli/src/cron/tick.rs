//! One scheduler pass (`tole cron tick`, #579).
//!
//! 1. Under the host lock (milliseconds): `recover` claims whose lease is
//!    dead, compute what is `ready`, record `missed`/`blocked` and `claim`
//!    each due occurrence (the claim is durable before anything runs).
//! 2. Without the lock: execute the claimed runs, at most
//!    `max_concurrent` at a time.
//! 3. Under the lock again: `settle` each run, then record its delivery.
//!
//! A crash anywhere leaves either no claim (nothing ran) or a claim that
//! the next tick's `recover` turns into `interrupted` — never a re-run.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use serde_json::json;
use tole_core::ledger::{DeliveryStatus, Fire, Job, LedgerError, RunStatus};

use super::{field, mode, store, DEFAULT_TIMEOUT_SECS};

/// Largest run output kept (bytes); the rest is cut with a marker.
const MAX_OUTPUT: usize = 1 << 20;

/// Tick tuning.
#[derive(Debug, Clone)]
pub struct TickOptions {
    pub max_concurrent: usize,
}

/// What one tick did.
#[derive(Debug, Clone, Default)]
pub struct TickReport {
    pub recovered: usize,
    pub missed: u64,
    pub blocked: usize,
    /// `(fire_id, status)` of every run this tick executed.
    pub ran: Vec<(String, RunStatus)>,
}

/// Where a run's output is kept (`<dir>/runs/<fire id, path-safe>`).
pub fn output_path(dir: &Path, fire_id: &str) -> PathBuf {
    let safe: String = fire_id
        .chars()
        .map(|c| match c {
            'a'..='z' | 'A'..='Z' | '0'..='9' | '-' | '_' | '.' => c,
            _ => '_',
        })
        .collect();
    dir.join("runs").join(format!("{safe}.out"))
}

/// One scheduler pass at `now` (Unix seconds).
pub fn tick(dir: &Path, now: i64, opts: &TickOptions) -> anyhow::Result<TickReport> {
    let lease = store::own_lease();
    let mut report = TickReport::default();
    let claimed: Vec<(Fire, Job)> = store::with_lock(dir, |l| {
        report.recovered = l.recover(now, store::lease_alive)?.len();
        let mut claimed = Vec::new();
        for due in l.ready(now)? {
            report.missed += due.missed.as_ref().map_or(0, |m| m.count);
            let Some(job) = l.state().job(&due.job_id).map(|j| j.job.clone()) else {
                continue;
            };
            if due.fire.is_some() && mode(&job) != "script" {
                l.block(
                    &due,
                    "agent_mode_unavailable",
                    Some("agent runs land in a follow-up of #579; nothing was spent".into()),
                    now,
                )?;
                report.blocked += 1;
                continue;
            }
            match l.claim(&due, lease, now) {
                Ok(Some(fire)) => claimed.push((fire, job)),
                Ok(None) => {}
                // Another tick got there first: nothing to do here.
                Err(LedgerError::AlreadyClaimed { .. } | LedgerError::LostRace { .. }) => {}
                Err(LedgerError::AlreadyAccounted { .. } | LedgerError::JobInactive(_)) => {}
                Err(e) => return Err(e.into()),
            }
        }
        Ok(claimed)
    })?;

    let started = Instant::now();
    let results = run_bounded(claimed, opts.max_concurrent, |(fire, job)| {
        let out = run_script(&job);
        (fire, out)
    });

    let end = now + started.elapsed().as_secs() as i64;
    std::fs::create_dir_all(dir.join("runs"))?;
    for (fire, out) in &results {
        std::fs::write(output_path(dir, &fire.fire_id), &out.stdout)?;
    }
    store::with_lock(dir, |l| {
        for (fire, out) in results {
            let usage = json!({
                "tokens": 0,
                "exit_code": out.exit_code,
                "duration_ms": out.duration_ms,
                "error": out.error,
            });
            let done = l.settle(&fire.fire_id, out.status, None, Some(usage), end)?;
            let silent = out.stdout.trim().is_empty()
                || out.stdout.lines().next().map(str::trim) == Some("[SILENT]");
            let delivery = if silent {
                DeliveryStatus::Skipped
            } else {
                DeliveryStatus::Sent
            };
            l.deliver(&fire.fire_id, "log", delivery, None, end)?;
            report.ran.push((fire.fire_id, done.status));
        }
        Ok(())
    })?;
    Ok(report)
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
    let fail = |error: String| ScriptOutput {
        status: RunStatus::Error,
        stdout: String::new(),
        exit_code: None,
        duration_ms: start.elapsed().as_millis(),
        error: Some(error),
    };
    let line = field(job, "script").unwrap_or_default();
    let argv = match tole_core::run_command::RunCommandTool::split_argv(line) {
        Ok(a) if !a.is_empty() => a,
        Ok(_) => return fail("empty script".into()),
        Err(e) => return fail(e),
    };
    // Re-checked at run time: the log may have been edited by hand.
    if let Err(e) = tole_core::subprocess::check_destructive_argv(&argv) {
        return fail(e);
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
        Err(e) => fail(e),
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
