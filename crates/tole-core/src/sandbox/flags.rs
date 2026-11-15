//! What a `--sandbox <profile>` or `--no-sandbox` flag changed for this run
//! (#750, design section 6).
//!
//! The host (the CLI) decides whether a flag may be honoured and applies a
//! profile override to the [`super::Policy`] it sets. This module keeps the
//! two facts every other part needs: the note that says a flag overrode the
//! config (startup log, `tole doctor`), and the `--no-sandbox` switch that
//! [`super::policy`] consults. That switch is deliberately not part of the
//! stored policy: [`super::policy`] ignores it once the process is marked
//! unattended, so even a flag that slipped through a host check can never
//! turn the sandbox off for a cron job, a `tole serve` session or any other
//! run without a human.

use super::{unattended, Mode};
use std::sync::OnceLock;

/// A per-run flag override, set at most once per process.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Override {
    /// `--no-sandbox`; `configured` is the mode the config asked for.
    NoSandbox { configured: Mode },
    /// `--sandbox <profile>`: `name` replaced `configured` as the profile
    /// of every stdio server for this run.
    Profile { name: String, configured: String },
}

static OVERRIDE: OnceLock<Override> = OnceLock::new();

/// Records the override. A second call with the same value is fine; a
/// different one is refused, like [`super::set_policy`].
pub fn set(o: Override) -> Result<(), String> {
    if *OVERRIDE.get_or_init(|| o.clone()) == o {
        Ok(())
    } else {
        Err("a sandbox flag override is already set for this process and differs".into())
    }
}

/// The recorded override, if any.
pub fn current() -> Option<Override> {
    OVERRIDE.get().cloned()
}

/// True while `--no-sandbox` is in force: set, and the run is attended.
pub fn no_sandbox_active() -> bool {
    matches!(OVERRIDE.get(), Some(Override::NoSandbox { .. })) && !unattended()
}

fn mode_word(m: Mode) -> &'static str {
    match m {
        Mode::Off => "off",
        Mode::Optional => "optional",
        Mode::Required => "required",
    }
}

/// The line that says a flag overrode the config, for the startup log and
/// `tole doctor`. `None` when no flag was used.
pub fn note() -> Option<String> {
    Some(match OVERRIDE.get()? {
        Override::NoSandbox { configured } if unattended() => format!(
            "sandbox: --no-sandbox is ignored in an unattended run; the configured mode {} applies",
            mode_word(*configured)
        ),
        Override::NoSandbox { configured } => format!(
            "sandbox: --no-sandbox overrides the config (mode {}) for this run; stdio MCP servers \
             start unsandboxed",
            mode_word(*configured)
        ),
        Override::Profile { name, configured } => format!(
            "sandbox: --sandbox {name} overrides the config (profile {configured}) for every \
             stdio MCP server in this run"
        ),
    })
}
