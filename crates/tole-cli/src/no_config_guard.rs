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
//! The session marker is an environment variable: that part covers every
//! process tole itself started (child agents, delegated children, jobs) and
//! is defence in depth against a script that strips it, like the other #684
//! guards. So there is a second rule that does not depend on the marker (#867):
//! when the USER config sets `[sandbox]` (mode, profiles, consumers) or
//! `[secrets]`, `--no-config` also needs a terminal (stdin AND stderr), the same
//! rule as `--no-sandbox`. A script that strips the marker (`env -i`) has no
//! terminal, and a deliberate pty is the one thing that gets past it. When the
//! user config sets none of those keys there is nothing to protect and owner
//! scripts and CI keep working exactly as before. The user config is read only
//! to decide, fail closed: a file that cannot be inspected or parsed keeps the
//! strict behaviour. NOT a boundary (the marker and the terminal can both be
//! faked); the boundary is the command sandbox, which masks the data dir
//! (`[sandbox.consumers] run_command = "required"`). No new marker is added.

use crate::config::Config;
use std::path::Path;

/// What the host knows about this run.
#[derive(Debug, Clone)]
pub struct Context {
    /// A session started this process (`child_env::under_session`).
    pub under_session: bool,
    /// The subcommand is never attended (`serve`, `acp`, `mcp`).
    pub unattended_command: bool,
    /// stdin and stderr are both terminals (the `--no-sandbox` rule).
    pub tty: bool,
    /// What the USER config protects, as text for the refusal (`[sandbox]`,
    /// `[secrets]`, or that it could not be read); `None` when it sets none of
    /// them or does not exist. Only needs to be filled when `--no-config` is
    /// given.
    pub user_protects: Option<String>,
}

/// The policy keys of a user config that `--no-config` would drop and that a
/// terminal is therefore required for (#867): the `[sandbox]` table (mode,
/// profiles, servers, consumers) and a non-empty `[secrets]` table (anything
/// but the default release).
pub fn protected_keys(cfg: &Config) -> Option<String> {
    let mut keys: Vec<&str> = Vec::new();
    if cfg.sandbox.is_some() {
        keys.push("[sandbox]");
    }
    if cfg.secrets.as_ref().is_some_and(|s| !s.is_empty()) {
        keys.push("[secrets]");
    }
    (!keys.is_empty()).then(|| keys.join(" and "))
}

/// Reads the user config at `path` to decide, fail closed: `None` only when
/// there is no file (or no path) or the file sets none of the protected keys;
/// a file that cannot be inspected or does not parse is reported as protected
/// (the strict behaviour).
pub fn user_config_protects(path: Option<&Path>) -> Option<String> {
    let path = path?;
    match crate::config::user_config_present(path) {
        Ok(false) => None,
        Err(_) => Some("a user config that cannot be inspected".into()),
        Ok(true) => match crate::config::load(path) {
            Ok(cfg) => protected_keys(&cfg),
            Err(_) => Some("a user config that cannot be read or parsed".into()),
        },
    }
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
    if let (false, Some(what)) = (ctx.tty, &ctx.user_protects) {
        return Err(format!(
            "--no-config needs a terminal (stdin and stderr) because the user config sets \
             {what}, which it would drop; run it yourself from a terminal, or remove the \
             flag, or change the user config (issue #867)"
        ));
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
            tty: true,
            user_protects: None,
        }
    }

    fn protected() -> Option<String> {
        Some("[sandbox]".into())
    }

    #[test]
    fn no_flag_is_never_refused() {
        for (s, u) in [(false, false), (true, false), (false, true), (true, true)] {
            let c = Context {
                under_session: s,
                unattended_command: u,
                tty: false,
                user_protects: protected(),
            };
            assert_eq!(check(false, &c), Ok(()));
        }
    }

    #[test]
    fn an_attended_owner_keeps_the_flag() {
        assert_eq!(check(true, &ctx()), Ok(()));
        // At a terminal even a protected user config does not stop it.
        assert_eq!(
            check(
                true,
                &Context {
                    user_protects: protected(),
                    ..ctx()
                }
            ),
            Ok(())
        );
    }

    /// #867: without a terminal the flag is refused when the user config sets a
    /// protected key (no marker needed), and works when it sets none.
    #[test]
    fn without_a_terminal_it_is_refused_only_when_the_user_config_protects_something() {
        let no_tty = Context {
            tty: false,
            ..ctx()
        };
        assert_eq!(check(true, &no_tty), Ok(()));
        let e = check(
            true,
            &Context {
                user_protects: Some("[sandbox] and [secrets]".into()),
                ..no_tty.clone()
            },
        )
        .unwrap_err();
        assert!(
            e.contains("--no-config needs a terminal")
                && e.contains("[sandbox] and [secrets]")
                && e.contains("run it yourself from a terminal")
                && e.ends_with("(issue #867)"),
            "{e}"
        );
    }

    #[test]
    fn the_protected_keys_are_sandbox_and_a_non_empty_secrets_table() {
        let parse = |s: &str| crate::config::parse(s, std::path::Path::new("u.toml")).unwrap();
        assert_eq!(protected_keys(&parse("model = \"m\"\n")), None);
        assert_eq!(
            protected_keys(&parse("[sandbox]\nmode = \"off\"\n")).as_deref(),
            Some("[sandbox]")
        );
        assert_eq!(
            protected_keys(&parse("[sandbox.consumers]\nrun_command = \"required\"\n")).as_deref(),
            Some("[sandbox]")
        );
        assert_eq!(
            protected_keys(&parse(
                "[sandbox]\nmode = \"required\"\n[secrets]\nGITHUB_TOKEN = { tools = [\"gh\"] }\n"
            ))
            .as_deref(),
            Some("[sandbox] and [secrets]")
        );
    }

    /// Fail closed: a file that cannot be inspected or parsed keeps the strict
    /// behaviour, a missing one protects nothing.
    #[test]
    fn the_user_config_is_read_fail_closed() {
        let d = crate::test_dir::TestDir::new("tole-nocfg-read");
        assert_eq!(user_config_protects(None), None);
        assert_eq!(user_config_protects(Some(&d.join("absent.toml"))), None);
        let bad = d.join("bad.toml");
        std::fs::write(&bad, "this is [not toml\n").unwrap();
        assert!(user_config_protects(Some(&bad))
            .unwrap()
            .contains("cannot be read or parsed"));
        let file = d.join("a-file");
        std::fs::write(&file, "x").unwrap();
        assert!(user_config_protects(Some(&file.join("config.toml")))
            .unwrap()
            .contains("cannot be inspected"));
        let ok = d.join("ok.toml");
        std::fs::write(&ok, "[sandbox]\nmode = \"required\"\n").unwrap();
        assert_eq!(
            user_config_protects(Some(&ok)).as_deref(),
            Some("[sandbox]")
        );
        let plain = d.join("plain.toml");
        std::fs::write(&plain, "model = \"m\"\n").unwrap();
        assert_eq!(user_config_protects(Some(&plain)), None);
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
