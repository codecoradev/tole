//! `--no-config` is an owner-at-a-terminal flag (#855).
//!
//! It makes the project config and the USER config unread, so everything the
//! owner set there falls back to its default: the sandbox mode and
//! `[sandbox.consumers]`, the secret release policy, `[sensitive_files]`,
//! `[delegation]`, hooks, profiles and the approval allow/trust lists. A
//! process a session started (`TOLE_UNDER_SESSION`, a child agent, #684)
//! must not be able to drop the owner's policy that way, exactly as it
//! cannot pass `--sandbox` / `--no-sandbox` (`sandbox_flags`), and an
//! unattended face (`serve`, `acp`, `mcp`) has no human to answer for it.
//! An owner at a terminal keeps the flag: it is a debugging tool.
//!
//! The session marker is an environment variable: this guard covers every
//! process tole itself started (child agents, delegated children, jobs) and
//! is defence in depth against a script that strips it, like the other #684
//! guards; the sandbox is the boundary for those.

/// What the host knows about this run.
#[derive(Debug, Clone, Copy)]
pub struct Context {
    /// A session started this process (`child_env::under_session`).
    pub under_session: bool,
    /// The subcommand is never attended (`serve`, `acp`, `mcp`).
    pub unattended_command: bool,
}

/// Decides `--no-config` (`given`). `Ok(())`: nothing to refuse. Every
/// refusal names the reason and the remedy.
pub fn check(given: bool, ctx: &Context) -> Result<(), String> {
    if !given {
        return Ok(());
    }
    if ctx.under_session {
        return Err(format!(
            "--no-config is refused under an agent session ({} or {} is set): it would drop the \
             owner's user config (sandbox, approvals, secret release); only the owner at a \
             terminal ignores the config files (issue #855)",
            tole_core::child_env::UNDER_SESSION,
            tole_core::child_env::DEPTH
        ));
    }
    if ctx.unattended_command {
        return Err(
            "--no-config is refused for `tole serve`, `tole acp` and `tole mcp`: they run \
             without a human, so the owner's user config (sandbox, approvals, secret release) \
             always applies; remove the flag, or change the config files (issue #855)"
                .into(),
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx() -> Context {
        Context {
            under_session: false,
            unattended_command: false,
        }
    }

    #[test]
    fn no_flag_is_never_refused() {
        for (s, u) in [(false, false), (true, false), (false, true), (true, true)] {
            let c = Context {
                under_session: s,
                unattended_command: u,
            };
            assert_eq!(check(false, &c), Ok(()));
        }
    }

    #[test]
    fn an_attended_owner_keeps_the_flag() {
        assert_eq!(check(true, &ctx()), Ok(()));
    }

    #[test]
    fn it_is_refused_under_an_agent_session_and_names_the_marker() {
        let e = check(
            true,
            &Context {
                under_session: true,
                ..ctx()
            },
        )
        .unwrap_err();
        assert!(
            e.contains("agent session") && e.contains("TOLE_UNDER_SESSION"),
            "{e}"
        );
        assert!(e.contains("TOLE_AGENT_DEPTH"), "{e}");
    }

    #[test]
    fn it_is_refused_for_the_unattended_faces() {
        let e = check(
            true,
            &Context {
                unattended_command: true,
                ..ctx()
            },
        )
        .unwrap_err();
        assert!(e.contains("serve") && e.contains("without a human"), "{e}");
    }
}
