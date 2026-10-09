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

#[test]
fn config_flags_collects_the_security_flags_and_the_subcommands_own_allow() {
    let f = |a: &[&str]| config_flags(&parse(a));
    let r = f(&[
        "--trust",
        "internal",
        "--on-pretool",
        "p",
        "--on-posttool",
        "q",
        "--on-turnend",
        "r",
        "--skill",
        "s/SKILL.md",
        "--plan-mode",
        "--no-skills",
        "run",
        "--allow",
        "write_*",
        "hi",
    ]);
    assert_eq!(r.trust, ["internal"]);
    assert_eq!(r.allow, ["write_*"]);
    assert_eq!(r.on_pretool, ["p"]);
    assert_eq!(r.on_posttool, ["q"]);
    assert_eq!(r.on_turnend, ["r"]);
    assert_eq!(r.skill, [PathBuf::from("s/SKILL.md")]);
    assert!(r.plan_mode && r.no_skills && !r.no_auto_mcp);
    // Each subcommand feeds ITS OWN --allow.
    assert_eq!(f(&["resume", "id", "--allow", "a"]).allow, ["a"]);
    assert_eq!(f(&["chat", "--allow", "c"]).allow, ["c"]);
    let m = f(&[
        "mission",
        "g",
        "--allow",
        "m",
        "--verify",
        "v",
        "--verify-timeout",
        "9",
    ]);
    assert_eq!(
        (m.allow, m.verify.as_deref(), m.verify_timeout),
        (vec!["m".to_string()], Some("v"), Some(9))
    );
    // Not given != the clap default: the timeout stays None.
    assert_eq!(f(&["mission", "g"]).verify_timeout, None);
    assert!(f(&["sessions"]).allow.is_empty());
    #[cfg(feature = "shell-tools")]
    assert_eq!(f(&["serve", "--allow", "s"]).allow, ["s"]);
    #[cfg(feature = "mcp")]
    {
        let r = f(&["--mcp-server", "a=b", "--no-auto-mcp", "sessions"]);
        assert_eq!(
            (r.mcp_server, r.no_auto_mcp),
            (vec!["a=b".to_string()], true)
        );
    }
}

#[test]
fn refuse_adds_the_no_config_hint_only_for_config_values() {
    let plain = refuse("--skill is not supported by `tole serve`", false).to_string();
    assert_eq!(plain, "--skill is not supported by `tole serve`");
    let hinted = refuse("--skill is not supported by `tole serve`", true).to_string();
    assert!(hinted.starts_with("--skill is not supported by `tole serve`"));
    assert!(hinted.contains("use --no-config to ignore the project config"));
}
