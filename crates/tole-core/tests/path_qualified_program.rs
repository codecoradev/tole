//! #1286: a program written as a path is still refused (bare names resolve
//! through PATH, the audited surface), but the refusal says that and names
//! the bare name; the destructive text is kept for a teardown program or a
//! recursive `rm` behind the path, however it is wrapped.
#![cfg(feature = "shell-tools")]

use serde_json::json;
use tole_core::run_command::RunCommandTool;
use tole_core::subprocess::check_destructive_argv;
use tole_core::tool::Tool;

const DESTRUCTIVE: &str = "destructive beyond the Write risk tier";

fn s(v: &[&str]) -> Vec<String> {
    v.iter().map(|x| x.to_string()).collect()
}

#[test]
fn a_harmless_path_qualified_program_is_refused_with_its_own_reason() {
    for (argv, bare) in [
        (s(&["./target/release/tole", "discuss", "list"]), "tole"),
        (s(&["~/.local/bin/tole", "discuss", "list"]), "tole"),
        (s(&["/bin/echo", "hi"]), "echo"),
        (s(&["/usr/bin/git", "status"]), "git"),
        // The two refused calls of the issue: a path inside a shell payload.
        (
            s(&[
                "bash",
                "-c",
                "~/.local/bin/tole discuss list 2>&1 | head -20",
            ]),
            "tole",
        ),
        (
            s(&["sh", "-c", "/path/to/repo/target/release/tole discuss list"]),
            "tole",
        ),
        (s(&["env", "bash", "-c", "./tole discuss list"]), "tole"),
    ] {
        let err = check_destructive_argv(&argv).expect_err(&format!("must refuse: {argv:?}"));
        assert!(err.contains("written as a path"), "{argv:?}: {err}");
        assert!(err.contains(&format!("`{bare}`")), "{argv:?}: {err}");
        assert!(!err.contains(DESTRUCTIVE), "{argv:?}: {err}");
    }
}

#[test]
fn a_teardown_or_recursive_rm_behind_a_path_keeps_the_destructive_text() {
    for argv in [
        s(&["/bin/rm", "-rf", "/"]),
        s(&["/usr/bin/rm", "-r", "../outside"]),
        s(&["./rm", "-rf", "~"]),
        s(&["/bin/dd", "if=/dev/zero", "of=/dev/sda"]),
        s(&["/sbin/mkfs.ext4", "/dev/sda1"]),
        s(&["/sbin/shutdown", "-h", "now"]),
        s(&["/usr/bin/sudo", "rm", "-rf", "/"]),
        s(&["/usr/bin/env", "dd", "if=/dev/zero", "of=/dev/sda"]),
        s(&["/bin/bash", "-c", "rm -rf /etc"]),
        s(&["bash", "-c", "/bin/rm -rf /"]),
    ] {
        let err = check_destructive_argv(&argv).expect_err(&format!("must refuse: {argv:?}"));
        assert!(err.contains(DESTRUCTIVE), "{argv:?}: {err}");
    }
}

#[test]
fn every_path_qualified_program_is_still_refused() {
    for argv in [
        s(&["/bin/rm", "foo"]),
        s(&["/bin/rm", "-rf", "./build"]), // scoped, but written as a path
        s(&["/", "x"]),
        s(&["foo/", "x"]),
        s(&["-/bin/ls"]),
        s(&["/bin/bash", "-c", "ls"]),
        s(&["/usr/bin/env", "ls"]),
    ] {
        assert!(check_destructive_argv(&argv).is_err(), "{argv:?}");
    }
    // The bare form is the way out and still passes.
    assert!(check_destructive_argv(&s(&["tole", "discuss", "list"])).is_ok());
    assert!(check_destructive_argv(&s(&["bash", "-c", "tole discuss list | head -20"])).is_ok());
}

#[test]
fn run_command_gives_the_same_reasons() {
    let root = std::env::temp_dir();
    let t = RunCommandTool::new(root);
    let err = t
        .execute(json!({ "command": "./target/release/tole discuss list" }))
        .unwrap_err();
    assert!(
        err.contains("written as a path") && err.contains("`tole`"),
        "{err}"
    );
    let err = t
        .execute(json!({ "command": "/bin/rm -rf /" }))
        .unwrap_err();
    assert!(err.contains(DESTRUCTIVE), "{err}");
    let err = t
        .execute(json!({ "command": "bash -c '/bin/rm -rf /'" }))
        .unwrap_err();
    assert!(err.contains(DESTRUCTIVE), "{err}");
}
