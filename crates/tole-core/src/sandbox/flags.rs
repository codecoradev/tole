//! What a `--sandbox <profile>` or `--no-sandbox` flag changed for this run
//! (#750, design section 6).
//!
//! The host (the CLI) decides whether a flag may be honoured and records
//! the override here. The configured [`super::policy`] is never changed by
//! a flag: the owner's config is what trust and approval decisions read
//! (first-party names, the `plugin add` probe, the command consumers). Only
//! [`super::session_policy`], what this run's stdio MCP servers start under,
//! applies the override, and it ignores `--no-sandbox` once the process is
//! marked unattended, so even a flag that slipped through a host check can
//! never turn the sandbox off for a cron job, a `tole serve` session or any
//! other run without a human. The note says a flag overrode the config
//! (startup log, `tole doctor`).

use super::{unattended, Mode, Policy, Profile};
use std::sync::OnceLock;

/// A per-run flag override, set at most once per process.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Override {
    /// `--no-sandbox`; `configured` is the mode the config asked for.
    NoSandbox { configured: Mode },
    /// `--sandbox <profile>`: `name` replaced `configured` as the profile
    /// of every stdio server for this run.
    Profile {
        name: String,
        profile: Profile,
        configured: String,
    },
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

/// The configured policy `p` as THIS RUN's stdio MCP servers see it
/// ([`super::session_policy`]): `--no-sandbox` (attended only) reads as no
/// policy, `--sandbox <p>` replaces the profile of every server. Nothing
/// else reads this: the configured policy stays what the owner wrote.
pub fn apply(p: Option<Policy>) -> Option<Policy> {
    match OVERRIDE.get() {
        Some(Override::NoSandbox { .. }) if !unattended() => None,
        Some(Override::Profile { name, profile, .. }) => p.map(|mut p| {
            p.profile_name = name.clone();
            p.profile = profile.clone();
            p.servers.clear();
            p
        }),
        _ => p,
    }
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
        Override::Profile {
            name, configured, ..
        } => format!(
            "sandbox: --sandbox {name} overrides the config (profile {configured}) for every \
             stdio MCP server in this run"
        ),
    })
}
