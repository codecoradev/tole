use super::*;

fn parse(args: &[&str]) -> Cli {
    let mut v = vec!["tole"];
    v.extend_from_slice(args);
    Cli::try_parse_from(v).unwrap()
}

fn policy(args: &[&str]) -> GatePolicy {
    gate_policy(&parse(args).command)
}

#[test]
fn skip_list_is_config_upgrade_approvals() {
    assert_eq!(policy(&["config", "check"]), GatePolicy::Skip);
    assert_eq!(policy(&["upgrade", "--check"]), GatePolicy::Skip);
    assert_eq!(policy(&["approvals", "list"]), GatePolicy::Skip);
}

#[test]
fn only_interactive_cli_commands_may_prompt() {
    assert_eq!(policy(&["run", "hi"]), GatePolicy::MayPrompt);
    assert_eq!(policy(&["chat"]), GatePolicy::MayPrompt);
    assert_eq!(policy(&["resume", "id"]), GatePolicy::MayPrompt);
    assert_eq!(policy(&["mission", "goal"]), GatePolicy::MayPrompt);
}

#[test]
fn stdin_as_prompt_and_non_interactive_commands_never_prompt() {
    assert_eq!(policy(&["run", "--prompt-file", "-"]), GatePolicy::NoPrompt);
    assert_eq!(
        policy(&["run", "--prompt-file", "p.txt"]),
        GatePolicy::MayPrompt
    );
    assert_eq!(policy(&["sessions"]), GatePolicy::NoPrompt);
    assert_eq!(policy(&["status", "x"]), GatePolicy::NoPrompt);
    #[cfg(feature = "shell-tools")]
    {
        assert_eq!(policy(&["serve"]), GatePolicy::NoPrompt);
        assert_eq!(policy(&["acp"]), GatePolicy::NoPrompt);
    }
    #[cfg(all(feature = "mcp", feature = "shell-tools"))]
    assert_eq!(policy(&["mcp"]), GatePolicy::NoPrompt);
}

#[test]
fn config_flags_collects_the_command_specific_values() {
    let f = |a: &[&str]| config_flags(&parse(a));
    assert_eq!(
        f(&["run", "--system", "S", "hi"]).system.as_deref(),
        Some("S")
    );
    let m = f(&["mission", "g", "--max-steps", "4", "--max-tokens", "9"]);
    assert_eq!(
        (m.max_steps, m.max_minutes, m.max_tokens),
        (Some(4), None, Some(9))
    );
    assert_eq!(
        f(&["--sessions-dir", "d", "sessions"])
            .sessions_dir
            .as_deref(),
        Some("d")
    );
    assert_eq!(f(&["sessions"]).system, None);
}

#[test]
fn config_flag_is_global_and_conflicts_with_no_config() {
    let c = parse(&["config", "check", "--config", "p.toml"]);
    assert_eq!(c.config.as_deref(), Some(Path::new("p.toml")));
    let c = parse(&["--config", "p.toml", "config", "check"]);
    assert_eq!(c.config.as_deref(), Some(Path::new("p.toml")));
    assert!(Cli::try_parse_from(["tole", "--no-config", "--config", "x", "sessions"]).is_err());
}
