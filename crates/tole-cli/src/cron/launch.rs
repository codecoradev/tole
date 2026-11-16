//! The one launcher of everything cron spawns from a job: a script job
//! ([`super::tick::run_script`]) and a `command:` delivery
//! ([`super::deliver`]) (#748 slice 3). Nothing else in `cron` starts a
//! process, so there is no path around the sandbox.
//!
//! With `[sandbox.consumers] cron` off (the default) the command is the plain
//! one, as before. With it `optional` or `required` the command runs in the
//! `command` profile, with the job's `workdir` as the workspace (see
//! [`tole_core::sandbox::command`]): the workdir writable except its `.git`,
//! `.tole`, `.tole-worktrees` and `.gitmodules` paths, the network as
//! `[sandbox.consumers] network` says, no `$HOME`, no tole data dir, the
//! rlimits of the profile. A cron job fires from a scheduler with nobody to
//! answer, so `optional` counts as `required`
//! ([`Consumer::always_unattended`]): a missing backend, a workdir or layout
//! the profile refuses and a launcher that does not start are REFUSALS, never
//! a run without the sandbox.

use std::path::PathBuf;
use std::process::{Command, Output};

use tole_core::ledger::Job;
use tole_core::sandbox::command::{command_in_dir, consumers, Consumer, LauncherReport};
use tole_core::sandbox::Mode;

use super::field;

/// A command ready to spawn, with what is needed to tell the launcher's
/// failure from the command's.
pub struct Prepared {
    pub cmd: Command,
    /// Set when the command is wrapped in the sandbox.
    report: Option<LauncherReport>,
}

impl Prepared {
    /// True when the command runs inside the sandbox.
    pub fn sandboxed(&self) -> bool {
        self.report.is_some()
    }

    /// Text for a spawn failure: the sandbox's own when wrapped.
    pub fn spawn_failure(&self, err: String) -> String {
        if self.sandboxed() && err.starts_with(tole_core::subprocess::SPAWN_FAILED) {
            format!("cron: not run, the sandbox did not start ({err})")
        } else {
            err
        }
    }

    /// `Some(reason)` when a finished process was the LAUNCHER failing, not
    /// the command: a non-zero exit for which `bwrap` reported no exit code of
    /// the command (it did not start, or did not end). That is a refusal, never
    /// the command's verdict and never a pass. `None` for an unsandboxed
    /// command, a success and a real exit code of the command.
    pub fn launcher_failure(self, out: &Output) -> Option<String> {
        let report = self.report?;
        let code = out.status.code()?;
        if code == 0 || report.command_exit_code() == Some(code) {
            return None;
        }
        let tail: String = String::from_utf8_lossy(&out.stderr)
            .trim()
            .chars()
            .take(800)
            .collect();
        Some(format!(
            "cron: not run, the sandbox failed before or while starting the command \
             (bwrap exit {code}): {tail}"
        ))
    }
}

/// The process for `argv` run by `job`: the plain command in the job's
/// workdir (or the working directory when it has none) when the sandbox is
/// off, else `bwrap ... -- argv` with the workdir as the workspace. The
/// environment is scrubbed as a cron child always was. `Err` is the refusal,
/// ready to record.
pub fn prepare(job: &Job, argv: &[String]) -> Result<Prepared, String> {
    let on = consumers().mode(Consumer::Cron) != Mode::Off;
    let workdir = field(job, "workdir").map(PathBuf::from);
    // The directory the command runs in, which is what the sandbox binds. A
    // job without a workdir runs where `tole cron` runs, as it always did; if
    // that cannot be told the sandbox has nothing to bind: refused.
    let ws = match (&workdir, on) {
        (Some(w), _) => w.clone(),
        (None, true) => std::env::current_dir()
            .map_err(|e| format!("cron: not run, the working directory is unknown ({e})"))?,
        (None, false) => PathBuf::new(),
    };
    let launched = command_in_dir(Consumer::Cron, &ws, &argv[0], &argv[1..])?;
    let mut cmd = launched.cmd;
    if let Some(w) = &workdir {
        // Also the launcher's own directory when wrapped; a workdir that does
        // not exist fails the spawn, as before, and is a refusal when wrapped.
        cmd.current_dir(w);
    }
    tole_core::subprocess::scrub_env_for(&mut cmd, "cron");
    Ok(Prepared {
        cmd,
        report: launched.report,
    })
}
