//! Owner-only subcommands refuse to write under a session (#684).
//!
//! `tole cron create|resume|edit`, `tole plugin approve`, `tole skills
//! approve` (and `skills drafts promote`) and `tole config trust` record
//! "the owner did this". A process a session started (`run_command`,
//! `job_start`, a child agent) carries `TOLE_UNDER_SESSION` /
//! `TOLE_AGENT_DEPTH` (`tole_core::child_env::under_session`); the guard
//! refuses before anything is written. A human running `tole` from a
//! terminal has neither, so nothing changes for them.
//!
//! Defense in depth, not a boundary: a copied binary run with a cleared
//! environment, or an interpreter editing the files, is not stopped here
//! (`docs/threat-model.md`, #601).

/// The refusal for `what` when `env` says this process runs under a session.
pub fn check(env: &dyn Fn(&str) -> Option<String>, what: &str) -> Result<(), String> {
    if tole_core::child_env::under_session(env) {
        return Err(format!(
            "`{what}` records an owner decision and refuses to run under an agent session \
             ({} or {} is set); run it yourself from a terminal (issue #684)",
            tole_core::child_env::UNDER_SESSION,
            tole_core::child_env::DEPTH
        ));
    }
    Ok(())
}

/// [`check`] against the real environment.
pub fn refuse_under_session(what: &str) -> Result<(), String> {
    check(&crate::config_apply::real_env, what)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env<'a>(k: &'a str, v: &'a str) -> impl Fn(&str) -> Option<String> + 'a {
        move |n: &str| (n == k).then(|| v.to_string())
    }

    #[test]
    fn a_terminal_passes_and_a_session_is_refused() {
        assert!(check(&|_: &str| None, "tole cron create").is_ok());
        assert!(check(&env("TOLE_UNDER_SESSION", "1"), "tole cron create").is_err());
        assert!(check(&env("TOLE_AGENT_DEPTH", "1"), "tole cron resume").is_err());
        assert!(check(&env("TOLE_AGENT_DEPTH", "0"), "x").is_ok());
        assert!(check(&env("TOLE_UNDER_SESSION", ""), "x").is_ok());
    }
}
