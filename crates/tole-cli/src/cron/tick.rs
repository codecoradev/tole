//! One scheduler pass (`tole cron tick`, #579).
//!
//! 1. Under the host lock (milliseconds): `recover` claims whose lease is
//!    dead, compute what is `ready`, record `missed`/`blocked` and `claim`
//!    each due occurrence (the claim is durable before anything runs).
//! 2. Without the lock: execute the claimed runs, at most
//!    `max_concurrent` at a time — script jobs as argv, agent jobs as one
//!    durable session each (see [`super::agent`]).
//! 3. Under the lock again: `settle` every run, then record its delivery.
//!
//! A crash anywhere leaves either no claim (nothing ran) or a claim that
//! the next tick's `recover` turns into `interrupted` — never a re-run.
//! Run output is passed through the `SecretSet` redactor (#595) before it
//! is written to disk.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use tole_core::ledger::{DeliveryStatus, Fire, Job, LedgerError, RunStatus};

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
}

impl TickOptions {
    /// A standalone tick (no approvals queue).
    pub const fn standalone(max_concurrent: usize) -> Self {
        Self {
            max_concurrent,
            approvals: None,
        }
    }
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

/// One finished run, ready to settle.
struct RunResult {
    status: RunStatus,
    session_id: Option<String>,
    output: String,
    usage: Value,
    target: &'static str,
}

fn execute(job: &Job, opts: &TickOptions) -> RunResult {
    if mode(job) == "script" {
        let out = run_script(job);
        return RunResult {
            status: out.status,
            session_id: None,
            usage: json!({
                "tokens": 0,
                "exit_code": out.exit_code,
                "duration_ms": out.duration_ms,
                "error": out.error,
            }),
            output: out.stdout,
            target: "log",
        };
    }
    let started = Instant::now();
    let run = super::agent::run_agent(job, opts.approvals.as_ref());
    RunResult {
        status: run.status,
        usage: json!({
            "duration_ms": started.elapsed().as_millis(),
            "error": run.error,
            "approvals": if opts.approvals.is_some() { "parked" } else { "refused" },
        }),
        session_id: run.session_id,
        output: run.text,
        target: "session",
    }
}

/// One scheduler pass at `now` (Unix seconds).
pub fn tick(dir: &Path, now: i64, opts: &TickOptions) -> anyhow::Result<TickReport> {
    let lease = store::own_lease();
    let mut report = TickReport::default();
    let provider_ready = crate::turn_runner::provider_config().is_ok();
    let claimed: Vec<(Fire, Job)> = store::with_lock(dir, |l| {
        report.recovered = l.recover(now, store::lease_alive)?.len();
        let mut claimed = Vec::new();
        for due in l.ready(now)? {
            report.missed += due.missed.as_ref().map_or(0, |m| m.count);
            let Some(job) = l.state().job(&due.job_id).map(|j| j.job.clone()) else {
                continue;
            };
            // Pre-dispatch validation (P10): an agent job without a
            // provider is blocked before it can spend anything.
            if due.fire.is_some() && mode(&job) != "script" && !provider_ready {
                l.block(
                    &due,
                    "provider_missing",
                    Some(crate::turn_runner::MISSING_PROVIDER_CONFIG.into()),
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
        let out = execute(&job, opts);
        (fire, out)
    });
    let end = now + started.elapsed().as_secs() as i64;

    // Keep the output (redacted). A failed write is recorded on the run,
    // never a reason to leave a claim unsettled.
    let secrets = tole_core::secrets::SecretSet::from_env();
    let results: Vec<(Fire, RunResult)> = results
        .into_iter()
        .map(|(fire, mut r)| {
            r.output = secrets.redact(&r.output).into_owned();
            let path = output_path(dir, &fire.fire_id);
            let written = std::fs::create_dir_all(dir.join("runs"))
                .and_then(|()| std::fs::write(&path, &r.output));
            if let (Err(e), Some(u)) = (written, r.usage.as_object_mut()) {
                u.insert("output_error".into(), e.to_string().into());
            }
            (fire, r)
        })
        .collect();

    store::with_lock(dir, |l| {
        let mut first_err = None;
        for (fire, r) in results {
            let settled = l.settle(&fire.fire_id, r.status, r.session_id, Some(r.usage), end);
            let done = match settled {
                Ok(d) => d,
                Err(e) => {
                    first_err.get_or_insert(e);
                    continue;
                }
            };
            let silent = r.output.trim().is_empty()
                || r.output.lines().next().map(str::trim) == Some("[SILENT]");
            let delivery = if silent {
                DeliveryStatus::Skipped
            } else {
                DeliveryStatus::Sent
            };
            if let Err(e) = l.deliver(&fire.fire_id, r.target, delivery, None, end) {
                first_err.get_or_insert(e);
            }
            report.ran.push((fire.fire_id, done.status));
        }
        match first_err {
            Some(e) => Err(e.into()),
            None => Ok(()),
        }
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
