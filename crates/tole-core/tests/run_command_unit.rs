//! The command splitter, `describe` / `summary`, the destructive refusals and the
//! cwd jail of `run_command`, moved from `src/run_command.rs` (public API only)
//! so the crate's source stays under its size guard.
#![cfg(feature = "shell-tools")]

#[path = "support/test_dir.rs"]
mod test_dir;
use serde_json::json;
use std::path::PathBuf;
use test_dir::TestDir;
use tole_core::run_command::RunCommandTool;
use tole_core::tool::{Risk, Tool};

#[test]
fn catastrophic_argv_refused() {
    let t = RunCommandTool::new(PathBuf::from("/tmp"));
    for line in [
        "rm -rf /",
        "rm --recursive /",
        "rm -r ../outside",
        "rm -rf /etc",
        "rm -rf ~",
        "mkfs.ext4 /dev/sda1",
        "dd if=/dev/zero of=/dev/sda",
        "shutdown -h now",
        "/bin/rm -rf /",
        "sh -c 'rm -rf /etc'",
        "bash -lc 'dd if=/dev/zero of=/dev/sda'",
        "env FOO=1 rm -rf /",
    ] {
        let err = t
            .execute(json!({ "command": line }))
            .err()
            .unwrap_or_else(|| panic!("must refuse: {line}"));
        assert!(
            err.contains("destructive beyond the Write risk tier"),
            "{line}"
        );
    }
}

/// Token matching must not over-refuse benign commands whose ARGUMENT
/// TEXT merely contains destructive keywords (CodeCora #3), and
/// a path-qualified program inside a shell text is refused.
#[test]
fn benign_commands_with_destructive_looking_args_pass() {
    // ISOLATED jail (cora full-scan #18): this test EXECUTES
    // `rm -rf ./build` — running it against the SHARED temp dir
    // deleted any peer test's files living at <temp>/build. Each
    // run gets its own scratch jail with a `build` dir inside.
    let jail = TestDir::new("tole-run-benign");
    std::fs::create_dir_all(jail.join("build")).unwrap();
    std::fs::write(jail.join("middleware.ts"), "x").unwrap();
    std::fs::write(jail.join("runbook.md"), "x").unwrap();
    let t = RunCommandTool::new(jail.to_path_buf());
    for line in [
        "cat middleware.ts",
        "grep shutdown runbook.md",
        "git add file",
        "cargo add serde",
        "echo useradd",
    ] {
        let out = t.execute(json!({ "command": line }));
        assert!(out.is_ok(), "must NOT refuse: {line}");
    }
    // Scoped recursive rm inside the workspace stays allowed (Write
    // tier territory), including dot-segment noise that normalizes
    // inside.
    assert!(t.execute(json!({ "command": "rm -rf ./build" })).is_ok());
    assert!(t
        .execute(json!({ "command": "rm -rf ./a/../build" }))
        .is_ok());
    let _ = std::fs::remove_dir_all(&jail);
    // Path-qualified program in a shell text: refused (the shell resolves it).
    let err = t
        .execute(json!({ "command": "sh -c '/bin/echo hi'" }))
        .expect_err("path-qualified program must be refused");
    assert!(err.contains("written as a path"), "{err}");
}

#[test]
fn classified_write_never_destructive() {
    let t = RunCommandTool::new(PathBuf::from("/tmp"));
    assert_eq!(t.risk(), Risk::Write);
}

#[test]
fn split_basic_and_quoted() {
    let argv = RunCommandTool::split_argv("echo 'hello world' \"a \\\"b\\\"\" x\\ y").unwrap();
    assert_eq!(argv, vec!["echo", "hello world", "a \"b\"", "x y"]);
}

#[test]
fn split_empty_line_is_empty_argv() {
    assert!(RunCommandTool::split_argv("   ").unwrap().is_empty());
}

#[test]
fn split_unterminated_quotes_error() {
    assert!(RunCommandTool::split_argv("echo 'oops").is_err());
    assert!(RunCommandTool::split_argv("echo \"oops").is_err());
}

#[test]
fn describe_shows_exact_argv() {
    let t = RunCommandTool::new(PathBuf::from("/tmp"));
    let d = t.describe(&json!({"command": "ls -la 'my dir'"}));
    // Token boundaries preserved: the quoted arg renders re-quoted.
    assert_eq!(d, "run: ls -la 'my dir'");
}

#[test]
fn describe_quotes_ambiguous_tokens() {
    let t = RunCommandTool::new(PathBuf::from("/tmp"));
    // One arg WITH a space renders quoted...
    let one = t.describe(&json!({"command": "printf 'a b'"}));
    assert_eq!(one, "run: printf 'a b'");
    // ...differently from two args (the cora full-scan #16 point).
    let two = t.describe(&json!({"command": "printf a b"}));
    assert_eq!(two, "run: printf a b");
    assert_ne!(one, two);
    // Embedded single quotes widen to '\''; metacharacters quoted.
    let semi = t.describe(&json!({"command": "echo 'x; y'"}));
    assert_eq!(semi, r"run: echo 'x; y'");
    let meta = t.describe(&json!({"command": "echo a;b"}));
    assert_eq!(meta, r"run: echo 'a;b'");
}

/// The approval summary keeps the token boundaries `describe` pins
/// (cora full-scan #16) and flags a line that does not split.
#[test]
fn summary_quotes_ambiguous_tokens_like_describe() {
    let t = RunCommandTool::new(PathBuf::from("/tmp"));
    for line in [
        "printf 'a b'",
        "printf a b",
        "echo 'x; y'",
        "echo a;b",
        "sh -c 'echo ok; rm -rf x'",
        "ls -la 'my dir'",
    ] {
        let input = json!({ "command": line });
        let s = t.action_summary(&input).unwrap();
        let described = t.describe(&input);
        assert_eq!(format!("run: {}", s.target), described, "{line}");
        assert_eq!(s.preview, [format!("$ {}", s.target)], "{line}");
    }
    let one = t
        .action_summary(&json!({"command": "printf 'a b'"}))
        .unwrap();
    let two = t.action_summary(&json!({"command": "printf a b"})).unwrap();
    assert_ne!(one.target, two.target);
    assert_eq!(one.target, "printf 'a b'");
    let sh = t
        .action_summary(&json!({"command": "sh -c 'echo ok; rm -rf x'"}))
        .unwrap();
    assert_eq!(sh.target, "sh -c 'echo ok; rm -rf x'");
    let bad = t.action_summary(&json!({"command": "echo 'oops"})).unwrap();
    assert!(bad.target.contains("<unparsable command"), "{}", bad.target);
    assert!(
        bad.target.contains("echo 'oops"),
        "the raw line stays visible"
    );
}

#[test]
fn describe_unparsable_is_flagged() {
    let t = RunCommandTool::new(PathBuf::from("/tmp"));
    let d = t.describe(&json!({"command": "echo 'oops"}));
    assert!(d.contains("unparsable"), "{d}");
}

#[test]
fn execute_rejects_missing_and_empty() {
    let t = RunCommandTool::new(PathBuf::from("/tmp"));
    assert!(t.execute(json!({})).is_err());
    assert!(t.execute(json!({"command": "   "})).is_err());
}

#[test]
fn execute_runs_and_reports_status() {
    let t = RunCommandTool::new(std::env::temp_dir());
    let out = t.execute(json!({"command": "sh -c 'exit 3'"})).unwrap();
    assert_eq!(out["status"], 3);
}

#[test]
fn execute_cwd_is_jailed() {
    // The command sees the jail as its working directory.
    let jail = TestDir::new("tole-runcommand-jail");
    let t = RunCommandTool::new(jail.to_path_buf());
    let out = t.execute(json!({"command": "pwd"})).unwrap();
    // POSIX pwd prints the logical path; canonicalize both sides.
    let got = std::path::Path::new(out["stdout"].as_str().unwrap().trim())
        .canonicalize()
        .unwrap_or_default();
    let want = jail.canonicalize().unwrap_or_default();
    assert_eq!(got, want);
}

#[test]
fn execute_caps_streams_independently_and_flags() {
    let t = RunCommandTool::new(std::env::temp_dir());
    let out = t
        .execute(json!({"command": "sh -c 'echo ok; echo ok >&2'"}))
        .unwrap();
    assert_eq!(out["stdout_truncated"], false);
    assert_eq!(out["stderr_truncated"], false);
    assert_eq!(out["stdout"], "ok\n");
    // 3000 chars: over the stderr budget (2000), under stdout's (4000).
    let out = t
        .execute(
            json!({"command": "sh -c 'head -c 3000 /dev/zero | tr \"\\0\" x | tee /dev/stderr'"}),
        )
        .unwrap();
    assert_eq!(out["stdout_truncated"], false);
    assert_eq!(out["stderr_truncated"], true);
    assert!(out["stderr"].as_str().unwrap().chars().count() <= 2000);
}

#[test]
fn execute_long_output_keeps_head_and_tail() {
    let t = RunCommandTool::new(std::env::temp_dir());
    let cmd = r#"sh -c 'printf HEAD_MARK; head -c 40000 /dev/zero | tr "\0" y; printf TAIL_MARK'"#;
    let out = t.execute(json!({ "command": cmd })).unwrap();
    let s = out["stdout"].as_str().unwrap();
    assert!(
        s.starts_with("HEAD_MARK") && s.ends_with("TAIL_MARK"),
        "{s}"
    );
    assert!(s.contains("characters omitted"));
    assert_eq!(out["stdout_truncated"], true);
}

/// #598: a command gets no secret by default, `GITHUB_TOKEN` included;
/// an explicit grant to `run_command` restores that one token.
#[cfg(unix)]
#[test]
fn a_command_gets_no_secret_unless_one_is_granted_to_run_command() {
    use tole_core::secret_release::Release;
    std::env::set_var("GITHUB_TOKEN", "t598-gh-token-value");
    std::env::set_var("TOLE_T598_OTHER_TOKEN", "t598-other-value");
    let out = |t: &RunCommandTool, name: &str| {
        t.execute(json!({"command": format!("printenv {name}")}))
            .unwrap()["stdout"]
            .as_str()
            .unwrap()
            .to_string()
    };
    // The process release (the default here): nothing for commands.
    let mut t = RunCommandTool::new(std::env::temp_dir());
    assert_eq!(out(&t, "GITHUB_TOKEN"), "");
    t.release = Some(Release::default().with(
        "GITHUB_TOKEN",
        ["gh", "git", "run_command"].map(String::from),
    ));
    assert_eq!(out(&t, "GITHUB_TOKEN").trim(), "t598-gh-token-value");
    assert_eq!(out(&t, "TOLE_T598_OTHER_TOKEN"), "", "only the granted one");
}

/// #684: the tole binary is refused in EVERY session kind, not only
/// child agents (the default tool is a root session's).
#[cfg(unix)]
#[test]
fn root_session_refuses_the_tole_binary() {
    let dir = TestDir::new("tole-rc-684");
    let t = RunCommandTool::new(dir.to_path_buf());
    assert!(!t.child_agent_mode);
    for line in [
        "tole cron resume job-1",
        "env tole cron resume job-1",
        "sh -c 'tole plugin approve x --hash h'",
        "env -u TOLE_UNDER_SESSION printenv",
    ] {
        let err = t
            .execute(json!({ "command": line }))
            .err()
            .unwrap_or_else(|| panic!("must refuse: {line}"));
        assert!(err.contains("#684"), "{line}: {err}");
    }
    // Ordinary commands that mention the name still run.
    let ok = t.execute(json!({ "command": "echo tole" })).unwrap();
    assert_eq!(ok["status"], 0);
    let _ = std::fs::remove_dir_all(&dir);
}

/// #684: what a session starts carries the marker the owner-only
/// subcommands refuse on.
#[cfg(unix)]
#[test]
fn spawned_commands_carry_the_session_marker() {
    let dir = TestDir::new("tole-rc-684m");
    let t = RunCommandTool::new(dir.to_path_buf());
    let out = t
        .execute(json!({ "command": "sh -c 'printenv | grep ^TOLE_UNDER'" }))
        .unwrap();
    assert_eq!(out["status"], 0, "{out}");
    let env = out["stdout"].as_str().unwrap();
    assert!(env.lines().any(|l| l == "TOLE_UNDER_SESSION=1"), "{env}");
    let _ = std::fs::remove_dir_all(&dir);
}
