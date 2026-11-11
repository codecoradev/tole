//! Delivery targets of a cron run (#579, design §9). A run's outcome
//! (`done`) and each delivery (`delivery`) are separate records: a failed
//! delivery never changes the run status.
//!
//! - `log` (script default): the run's output file `runs/<fire>.out`,
//!   which `tole cron runs` and `GET /cron/runs` read.
//! - `session` (agent default): the run's own session transcript.
//! - `file:<path>`: append the output to `<path>`, relative to the job's
//!   workdir and kept inside it (no absolute path, no `..`, no symlinked
//!   target, the parent re-checked after canonicalizing).
//! - `command:<argv>`: run argv (no shell, scrubbed env, 60 s timeout,
//!   destructive argv refused) with the output on stdin and
//!   `TOLE_CRON_JOB`/`TOLE_CRON_FIRE`/`TOLE_CRON_STATUS` set. Trust-gated:
//!   only a job saved by a human through the CLI (`created_by = cli`) may
//!   run one; any other job is `blocked` (`delivery_untrusted`).
//!
//! Empty output, or a first line of `[SILENT]`, delivers nothing (every
//! target records `skipped`).

use std::io::Write;
use std::path::{Component, Path};
use std::process::Command;
use std::time::Duration;

use tole_core::ledger::{DeliveryStatus, Job, RunStatus};
use tole_core::secrets::SecretSet;

use super::{field, mode};

/// Ceiling of a `command:` delivery.
const COMMAND_TIMEOUT: Duration = Duration::from_secs(60);

/// One delivery target, as written in the job (`deliver[]`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    Log,
    Session,
    File(String),
    Command(String),
}

impl Target {
    /// Parses and checks the shape of a target (not the job-specific rules).
    pub fn parse(spec: &str) -> Result<Target, String> {
        match spec.split_once(':') {
            None if spec == "log" => Ok(Target::Log),
            None if spec == "session" => Ok(Target::Session),
            Some(("file", p)) => {
                let path = Path::new(p);
                if p.is_empty()
                    || path.is_absolute()
                    || path
                        .components()
                        .any(|c| !matches!(c, Component::Normal(_)))
                {
                    return Err(format!(
                        "file target `{p}` must be a relative path inside the workdir (no `..`)"
                    ));
                }
                Ok(Target::File(p.to_string()))
            }
            Some(("command", line)) => {
                let argv = tole_core::run_command::RunCommandTool::split_argv(line)?;
                if argv.is_empty() {
                    return Err("command target is empty".into());
                }
                tole_core::subprocess::check_destructive_argv(&argv)?;
                Ok(Target::Command(line.to_string()))
            }
            _ => Err(format!(
                "unknown delivery target `{spec}` (log, session, file:PATH or command:ARGV)"
            )),
        }
    }

    /// The text stored in the job and in each `delivery` record.
    pub fn spec(&self) -> String {
        match self {
            Target::Log => "log".into(),
            Target::Session => "session".into(),
            Target::File(p) => format!("file:{p}"),
            Target::Command(c) => format!("command:{c}"),
        }
    }
}

/// Validates the targets of a job being saved (`mode`: script or agent).
pub fn validate(mode: &str, specs: &[String]) -> Result<(), String> {
    for s in specs {
        if Target::parse(s)? == Target::Session && mode == "script" {
            return Err("a script job has no session: use log, file: or command:".into());
        }
    }
    Ok(())
}

/// The job's targets: its `deliver` list, else `session` (agent) / `log`
/// (script).
pub fn targets(job: &Job) -> Result<Vec<Target>, String> {
    let specs: Vec<String> = match job.extra.get("deliver") {
        Some(v) => serde_json::from_value(v.clone()).map_err(|e| format!("deliver: {e}"))?,
        None => Vec::new(),
    };
    if specs.is_empty() {
        return Ok(vec![if mode(job) == "script" {
            Target::Log
        } else {
            Target::Session
        }]);
    }
    validate(mode(job), &specs)?;
    specs.iter().map(|s| Target::parse(s)).collect()
}

/// Pre-dispatch check (P10): `Err((code, detail))` blocks the occurrence
/// before anything runs.
pub fn check(job: &Job) -> Result<(), (&'static str, String)> {
    let targets = targets(job).map_err(|e| ("delivery_invalid", e))?;
    let trusted = field(job, "created_by") == Some("cli");
    match targets.iter().find(|t| matches!(t, Target::Command(_))) {
        Some(t) if !trusted => Err((
            "delivery_untrusted",
            format!(
                "`{}` runs a command, which only a job saved with `tole cron create` may do",
                t.spec()
            ),
        )),
        _ => Ok(()),
    }
}

/// What a delivery is told about its run.
pub struct RunInfo<'a> {
    pub job: &'a Job,
    pub fire_id: &'a str,
    pub scheduled_for: i64,
    pub status: RunStatus,
    /// Redacted output.
    pub output: &'a str,
}

/// True when the output delivers nothing.
pub fn silent(output: &str) -> bool {
    output.trim().is_empty() || output.lines().next().map(str::trim) == Some("[SILENT]")
}

fn status_text(s: RunStatus) -> String {
    serde_json::to_value(s)
        .ok()
        .and_then(|v| v.as_str().map(str::to_string))
        .unwrap_or_default()
}

/// Delivers one run to one target. `log` and `session` are already
/// written by the time this runs, so they only report `sent`.
pub fn deliver(
    target: &Target,
    run: &RunInfo<'_>,
    secrets: &SecretSet,
) -> (DeliveryStatus, Option<String>) {
    if silent(run.output) {
        return (DeliveryStatus::Skipped, None);
    }
    let result = match target {
        Target::Log | Target::Session => Ok(()),
        Target::File(p) => append_file(run, p),
        Target::Command(line) => run_command(run, line),
    };
    match result {
        Ok(()) => (DeliveryStatus::Sent, None),
        Err(e) => (
            DeliveryStatus::Failed,
            Some(secrets.redact(&e).into_owned()),
        ),
    }
}

fn append_file(run: &RunInfo<'_>, rel: &str) -> Result<(), String> {
    let workdir = field(run.job, "workdir").unwrap_or_default();
    let root = Path::new(workdir)
        .canonicalize()
        .map_err(|e| format!("workdir {workdir}: {e}"))?;
    let path = root.join(rel);
    let parent = path.parent().unwrap_or(&root);
    std::fs::create_dir_all(parent).map_err(|e| format!("{rel}: {e}"))?;
    let parent = parent.canonicalize().map_err(|e| format!("{rel}: {e}"))?;
    if !parent.starts_with(&root) {
        return Err(format!("{rel}: resolves outside the workdir"));
    }
    let target = parent.join(path.file_name().unwrap_or_default());
    if std::fs::symlink_metadata(&target).is_ok_and(|m| m.file_type().is_symlink()) {
        return Err(format!("{rel}: is a symlink"));
    }
    let name = field(run.job, "name").unwrap_or(&run.job.id);
    let block = format!(
        "--- {name} {} {} ({})\n{}{}",
        super::fmt_utc(run.scheduled_for),
        status_text(run.status),
        run.fire_id,
        run.output,
        if run.output.ends_with('\n') { "" } else { "\n" }
    );
    std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&target)
        .and_then(|mut f| f.write_all(block.as_bytes()))
        .map_err(|e| format!("{rel}: {e}"))
}

fn run_command(run: &RunInfo<'_>, line: &str) -> Result<(), String> {
    let argv = tole_core::run_command::RunCommandTool::split_argv(line)?;
    tole_core::subprocess::check_destructive_argv(&argv)?;
    let mut cmd = Command::new(&argv[0]);
    cmd.args(&argv[1..]);
    if let Some(w) = field(run.job, "workdir") {
        cmd.current_dir(w);
    }
    tole_core::subprocess::scrub_env_for_child(&mut cmd);
    cmd.env("TOLE_CRON_JOB", &run.job.id)
        .env("TOLE_CRON_FIRE", run.fire_id)
        .env("TOLE_CRON_STATUS", status_text(run.status));
    let out = tole_core::subprocess::run_with_timeout_stdin(
        &mut cmd,
        COMMAND_TIMEOUT,
        run.output.as_bytes(),
    )?;
    if out.status.success() {
        return Ok(());
    }
    let err: String = String::from_utf8_lossy(&out.stderr)
        .chars()
        .take(200)
        .collect();
    Err(format!("exit {:?}: {}", out.status.code(), err.trim()))
}
