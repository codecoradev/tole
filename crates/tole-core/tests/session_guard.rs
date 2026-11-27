//! Unit-style tests of the session guard (#684). Moved out of `src/` (#1249):
//! they only use the public API of the crate.

#[path = "support/test_dir.rs"]
mod test_dir;

use std::path::Path;
use std::process::Command;
use tole_core::child_env::UNDER_SESSION;
use tole_core::session_guard::{check_session_argv, mark};

fn argv(line: &str) -> Vec<String> {
    tole_core::run_command::RunCommandTool::split_argv(line).unwrap()
}

fn refused(line: &str) -> bool {
    check_session_argv(&argv(line), Path::new("/tmp")).is_err()
}

#[test]
fn the_tole_binary_is_refused_in_every_form() {
    for line in [
        "tole cron resume job-1",
        "/usr/local/bin/tole plugin approve x --hash h",
        "./tole config trust --yes",
        "tole-cli skills approve s",
        "tole.exe cron create '@daily' --script x",
        "FOO=1 tole cron resume j",
        "env tole cron resume j",
        "env -u TOLE_FOO tole cron resume j",
        "env -i /opt/tole cron resume j",
        "nohup tole cron resume j",
        "timeout 5 tole cron resume j",
        "sh -c 'tole cron resume j'",
        "bash -lc 'cd x && tole cron resume j'",
        "sh -c 'echo hi; tole cron resume j'",
        "bash -c 'x=$(tole cron list)'",
        "sh -c 'if true; then tole cron resume j; fi'",
        "bash -c \"bash -c 'tole cron resume j'\"",
    ] {
        assert!(refused(line), "must refuse: {line}");
    }
}

#[test]
fn benign_commands_that_mention_tole_pass() {
    for line in [
        "git commit -m 'fix tole bug'",
        "echo tole",
        "grep -rn tole src",
        "sh -c 'echo tole'",
        "cat tole.toml",
        "cargo test -p tole-core",
        "ls tole-agents",
        "env FOO=1 printenv FOO",
        "sh -c 'cargo build -p tole-cli'",
    ] {
        assert!(!refused(line), "must allow: {line}");
    }
}

#[test]
fn the_marker_cannot_be_named() {
    for line in [
        "env -u TOLE_UNDER_SESSION printenv",
        "sh -c 'unset TOLE_UNDER_SESSION; ls'",
        "env TOLE_UNDER_SE''SSION= ls",
        "env TOLE_UNDER_SESSION= ls",
    ] {
        assert!(refused(line), "must refuse: {line}");
    }
}

#[cfg(unix)]
#[test]
fn a_link_to_the_running_executable_is_refused() {
    let dir = crate::test_dir::TestDir::new("tole-guard-link");
    let exe = std::env::current_exe().unwrap();
    std::os::unix::fs::symlink(&exe, dir.join("helper")).unwrap();
    assert!(check_session_argv(&["./helper".into(), "x".into()], &dir).is_err());
    let abs = dir.join("helper").display().to_string();
    assert!(check_session_argv(&[abs, "x".into()], &dir).is_err());
    assert!(check_session_argv(&["env".into(), "./helper".into()], &dir).is_err());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn mark_sets_the_marker() {
    let mut cmd = Command::new("true");
    mark(&mut cmd);
    assert!(cmd
        .get_envs()
        .any(|(k, v)| k == UNDER_SESSION && v.is_some()));
}
