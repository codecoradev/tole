//! `--sandbox <profile>` and `--no-sandbox` (#750, design section 6).
//!
//! Both flags are per run and only the owner at a terminal may pass them:
//!
//! - **Never from a session.** A process a session started (`TOLE_UNDER_SESSION`,
//!   a child agent) refuses both flags, whatever the config says (#684). An
//!   agent cannot pass them on its own behalf, and a script whose stdin or
//!   stderr is not a terminal cannot pass `--no-sandbox` either.
//! - **`--no-sandbox` never in an unattended run.** `tole serve`, `tole acp`
//!   and any run without a terminal count as unattended; there, a configured
//!   `required` (and `optional`, which counts as `required` without a human)
//!   mode is not switched off: the run is refused. Defense in depth in the
//!   core: [`tole_core::sandbox::policy`] ignores the flag once the process
//!   is marked unattended.
//! - **`--sandbox <profile>` only selects.** The profile must be built in or
//!   defined in the USER config (a project config cannot define one), it does
//!   not turn the sandbox on when the mode is `off`, and it never defines a
//!   profile. It replaces the profile of every stdio server for the run,
//!   per-server bindings included.
//! - **Only the session's servers.** The flags are an override the core
//!   applies to this run's stdio MCP servers (`sandbox::session_policy`);
//!   the configured policy (`sandbox::policy`: first-party names, the
//!   `plugin add` probe) and the command consumers never see them.
//!
//! The startup line and `tole doctor` say when a flag overrode the config
//! (`tole_core::sandbox::flags::note`).

use crate::config::SandboxConfig;
use tole_core::sandbox::flags::Override;
use tole_core::sandbox::{Mode, Policy};

/// The flags as typed.
#[derive(Debug, Clone, Default)]
pub struct Flags {
    pub sandbox: Option<String>,
    pub no_sandbox: bool,
}

impl Flags {
    pub fn any(&self) -> bool {
        self.sandbox.is_some() || self.no_sandbox
    }
}

/// What the host knows about this run.
#[derive(Debug, Clone, Copy)]
pub struct Context {
    /// A session started this process (`child_env::under_session`).
    pub under_session: bool,
    /// The subcommand is never attended (`serve`, `acp`).
    pub unattended_command: bool,
    /// The subcommand starts no server, so a sandbox flag means nothing
    /// (`cron`, `sandbox`, `hook`).
    pub meaningless: bool,
    /// stdin and stderr are terminals: a human can answer.
    pub tty: bool,
}

/// What an accepted flag changes: the override the core applies to this
/// run's stdio MCP servers (`sandbox::session_policy`). The configured
/// policy is never rewritten.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Outcome {
    pub over: Override,
}

fn word(m: Mode) -> &'static str {
    match m {
        Mode::Off => "off",
        Mode::Optional => "optional",
        Mode::Required => "required",
    }
}

/// Decides the flags against the configured policy (`base`, from the user
/// layer) and its table (`cfg`). `Ok(None)`: nothing to change. Every
/// refusal names the reason and the remedy.
pub fn apply(
    flags: &Flags,
    base: Option<&Policy>,
    cfg: Option<&SandboxConfig>,
    ctx: &Context,
) -> Result<Option<Outcome>, String> {
    if !flags.any() {
        return Ok(None);
    }
    let which = if flags.no_sandbox {
        "--no-sandbox"
    } else {
        "--sandbox"
    };
    if ctx.meaningless {
        return Err(format!(
            "{which} is not accepted here: this command starts no MCP server (set the sandbox in \
             the user config)"
        ));
    }
    if ctx.under_session {
        return Err(format!(
            "{which} is refused under an agent session ({} or {} is set): only the owner at a \
             terminal changes the sandbox (issue #684)",
            tole_core::child_env::UNDER_SESSION,
            tole_core::child_env::DEPTH
        ));
    }
    let mode = base.map_or(Mode::Off, |p| p.mode);
    if flags.no_sandbox {
        if mode == Mode::Off {
            return Ok(None);
        }
        if ctx.unattended_command || !ctx.tty {
            return Err(format!(
                "--no-sandbox is refused in an unattended run while the sandbox is {} (optional \
                 counts as required without a human); run it from a terminal, or change the \
                 mode in the user config",
                word(mode)
            ));
        }
        return Ok(Some(Outcome {
            over: Override::NoSandbox { configured: mode },
        }));
    }
    let name = flags.sandbox.as_deref().unwrap_or_default();
    let (Some(base), Some(cfg), true) = (base, cfg, mode != Mode::Off) else {
        return Err(
            "--sandbox selects a profile, it does not turn the sandbox on: set [sandbox] mode in \
             the user config first"
                .into(),
        );
    };
    // The user layer is the only place a profile comes from.
    let profile = cfg
        .resolve_profile(name)
        .map_err(|e| format!("--sandbox: {e}"))?;
    Ok(Some(Outcome {
        over: Override::Profile {
            name: name.to_string(),
            profile,
            configured: base.profile_name.clone(),
        },
    }))
}

#[cfg(test)]
#[path = "sandbox_flags_tests.rs"]
mod tests;
