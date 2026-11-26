//! The startup gates of a subcommand: how the config gate treats it, which
//! sandbox-flag class it is in, and what the command line supplied for the
//! config-applied keys.

use crate::cli_actions::DiscussAction;
use crate::{Cli, Command};

/// `(unattended, starts no server)` of a subcommand, for the sandbox flags
/// (#750): `serve`, `acp` and the MCP stdio server never have a human;
/// `cron`, `hook` and `sandbox` start no MCP server.
pub(crate) fn sandbox_flag_class(cmd: &Command) -> (bool, bool) {
    #[allow(unused_mut)]
    let mut class = (false, matches!(cmd, Command::Sandbox { .. }));
    // `tole plugin add|approve` probes and approves servers: never under a
    // run flag.
    #[cfg(feature = "mcp")]
    {
        class.1 |= matches!(cmd, Command::Plugin { .. });
    }
    #[cfg(feature = "shell-tools")]
    {
        class.0 |= matches!(cmd, Command::Serve { .. } | Command::Acp { .. });
        class.1 |= matches!(cmd, Command::Cron { .. } | Command::Hook { .. });
    }
    #[cfg(all(feature = "mcp", feature = "shell-tools"))]
    {
        class.0 |= matches!(cmd, Command::Mcp { .. });
    }
    class
}

/// How the startup config gate treats a command (#208).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum GatePolicy {
    /// `config`, `upgrade`, `setup`, `approvals`: no config is loaded.
    Skip,
    /// Interactive CLI commands: an untrusted config may be reviewed and
    /// approved at the terminal (still needs stdin AND stderr to be ttys).
    MayPrompt,
    /// Everything else (sessions/status, and the serve/acp/mcp protocol
    /// faces whose stdin/stdout are channels): fail closed, never ask.
    NoPrompt,
}

/// The subcommands that EXECUTE cron jobs in this process (`cron run`,
/// `cron tick`): they load the user layer (never the project config) although
/// `cron` is on the config skip list, because a job's agent run and script
/// must see the owner's policy (#863). The rest of `cron` only edits files and
/// keeps working when the config is broken.
pub(crate) fn runs_cron_jobs(cmd: &Command) -> bool {
    #[cfg(feature = "shell-tools")]
    if let Command::Cron { action } = cmd {
        use tole_cli::cron::CronAction as A;
        // No wildcard: a new `cron` subcommand must decide here whether it
        // executes jobs (and so needs the owner's policy) or only edits files.
        return match action {
            A::Run { .. } | A::Tick { .. } => true,
            A::Create { .. }
            | A::Edit { .. }
            | A::List
            | A::Status
            | A::Cancel { .. }
            | A::Pause { .. }
            | A::Resume { .. }
            | A::Remove { .. }
            | A::Runs { .. } => false,
        };
    }
    let _ = cmd;
    false
}

pub(crate) fn gate_policy(cmd: &Command) -> GatePolicy {
    match cmd {
        Command::Config { .. }
        | Command::Upgrade { .. }
        | Command::Setup { .. }
        | Command::Approvals { .. }
        | Command::Skills { .. }
        | Command::Curator { .. }
        | Command::Audit { .. }
        | Command::Delegation { .. }
        | Command::Sandbox { .. }
        | Command::Discuss {
            action: DiscussAction::List { .. } | DiscussAction::Show { .. },
        } => GatePolicy::Skip,
        #[cfg(feature = "shell-tools")]
        Command::Cron { .. } | Command::Hook { .. } => GatePolicy::Skip,
        Command::Webhook { .. } => GatePolicy::Skip,
        // `--prompt-file -` consumes stdin as the prompt: a question on
        // the same stream would eat prompt text.
        Command::Run { prompt_file, .. } if prompt_file.as_deref() == Some("-") => {
            GatePolicy::NoPrompt
        }
        Command::Run { .. }
        | Command::Chat { .. }
        | Command::Resume { .. }
        | Command::Mission { .. }
        | Command::Discuss { .. } => GatePolicy::MayPrompt,
        #[cfg(feature = "tui")]
        Command::Tui { .. } => GatePolicy::MayPrompt,
        _ => GatePolicy::NoPrompt,
    }
}

pub(crate) fn config_flags(cli: &Cli) -> tole_cli::config_apply::Flags {
    let mut f = tole_cli::config_apply::Flags {
        sessions_dir: cli.sessions_dir.clone(),
        workspace: cli.workspace.clone(),
        trust: cli.trust.clone(),
        on_pretool: cli.on_pretool.clone(),
        on_posttool: cli.on_posttool.clone(),
        on_turnend: cli.on_turnend.clone(),
        skill: cli.skill.clone(),
        plan_mode: cli.plan_mode,
        no_skills: cli.no_skills,
        compact_at: cli.compact_at,
        no_compaction: cli.no_compaction,
        reasoning_effort: cli.effort.clone(),
        ..Default::default()
    };
    #[cfg(feature = "shell-tools")]
    {
        f.memory = cli.memory.clone();
    }
    #[cfg(feature = "mcp")]
    {
        f.mcp_server = cli.mcp_server.clone();
        f.no_auto_mcp = cli.no_auto_mcp;
    }
    // Every subcommand with its own `--allow` feeds it here: the config
    // `allow` list is the fallback when THAT flag list is empty.
    match &cli.command {
        Command::Run {
            system,
            allow_patterns,
            ..
        }
        | Command::Chat {
            system,
            allow_patterns,
            ..
        } => {
            f.system = system.clone();
            f.allow = allow_patterns.clone();
        }
        Command::Resume { allow_patterns, .. } => f.allow = allow_patterns.clone(),
        #[cfg(feature = "tui")]
        Command::Tui { allow_patterns, .. } => f.allow = allow_patterns.clone(),
        Command::Mission {
            max_steps,
            max_minutes,
            max_tokens,
            verify,
            verify_timeout,
            allow_patterns,
            ..
        } => {
            f.max_steps = *max_steps;
            f.max_minutes = *max_minutes;
            f.max_tokens = *max_tokens;
            f.verify = verify.clone();
            f.verify_timeout = *verify_timeout;
            f.allow = allow_patterns.clone();
        }
        #[cfg(all(feature = "mcp", feature = "shell-tools"))]
        Command::Mcp { allow_patterns, .. } => f.allow = allow_patterns.clone(),
        #[cfg(feature = "shell-tools")]
        Command::Acp { allow_patterns, .. } | Command::Serve { allow_patterns, .. } => {
            f.allow = allow_patterns.clone()
        }
        _ => {}
    }
    f
}

/// #846: the table behind the `--sandbox` / `--no-sandbox` refusals. The
/// end-to-end tests have no terminal, so they cannot tell this
/// classification from "no terminal"; this pins it per subcommand:
/// `(unattended, starts no server)`.
#[cfg(test)]
mod sandbox_flag_class_tests {
    use super::{sandbox_flag_class, Cli};
    use clap::Parser;

    fn class(args: &[&str]) -> (bool, bool) {
        let mut argv = vec!["tole"];
        argv.extend_from_slice(args);
        sandbox_flag_class(&Cli::try_parse_from(argv).unwrap().command)
    }

    #[test]
    fn ordinary_commands_are_attended_and_start_servers() {
        assert_eq!(class(&["doctor"]), (false, false));
        assert_eq!(class(&["run", "hello"]), (false, false));
    }

    #[test]
    fn sandbox_and_plugin_start_no_session_server() {
        assert_eq!(class(&["sandbox", "list"]), (false, true));
        #[cfg(feature = "mcp")]
        assert_eq!(class(&["plugin", "list"]), (false, true));
    }

    #[cfg(feature = "shell-tools")]
    #[test]
    fn serve_and_acp_are_unattended() {
        assert_eq!(class(&["serve"]), (true, false));
        assert_eq!(class(&["acp"]), (true, false));
    }

    #[cfg(all(feature = "mcp", feature = "shell-tools"))]
    #[test]
    fn the_mcp_face_is_unattended() {
        assert_eq!(class(&["mcp"]), (true, false));
    }

    #[cfg(feature = "shell-tools")]
    #[test]
    fn cron_and_hook_start_no_server() {
        assert_eq!(class(&["cron", "tick"]), (false, true));
        assert_eq!(class(&["hook", "list"]), (false, true));
    }
}
