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
        (s(&["./target/release/mytool", "list"]), "mytool"),
        (s(&["/bin/echo", "hi"]), "echo"),
        (s(&["/usr/bin/git", "status"]), "git"),
        // A path inside a shell payload, as in the two refused calls of #1286.
        (
            s(&["bash", "-c", "~/.local/bin/mytool list 2>&1 | head -20"]),
            "mytool",
        ),
        (
            s(&["sh", "-c", "/path/to/repo/target/release/mytool list"]),
            "mytool",
        ),
        (s(&["env", "bash", "-c", "./mytool list"]), "mytool"),
    ] {
        let err = check_destructive_argv(&argv).expect_err(&format!("must refuse: {argv:?}"));
        assert!(err.contains("written as a path"), "{argv:?}: {err}");
        assert!(
            err.contains(&format!("use the bare name (`{bare}`)")),
            "{argv:?}: {err}"
        );
        assert!(!err.contains(DESTRUCTIVE), "{argv:?}: {err}");
    }
}

/// The tole binary cannot be run from a session at all (session guard,
/// #684): the refusal must not send the model to a bare `tole` that is
/// refused next.
#[test]
fn a_path_qualified_tole_is_not_told_to_use_the_bare_name() {
    for argv in [
        s(&["./target/release/tole", "discuss", "list"]),
        s(&["~/.local/bin/tole", "discuss", "list"]),
        s(&["/usr/local/bin/tole-cli", "run"]),
        s(&[
            "bash",
            "-c",
            "~/.local/bin/tole discuss list 2>&1 | head -20",
        ]),
        s(&["sh", "-c", "/path/to/repo/target/release/tole discuss list"]),
    ] {
        let err = check_destructive_argv(&argv).expect_err(&format!("must refuse: {argv:?}"));
        assert!(err.contains("written as a path"), "{argv:?}: {err}");
        assert!(
            err.contains("may not run the tole binary"),
            "{argv:?}: {err}"
        );
        assert!(!err.contains("use the bare name"), "{argv:?}: {err}");
    }
}

#[test]
fn the_echoed_program_is_capped_and_has_no_control_characters() {
    let long = format!("/{}\u{1b}[31m\nx", "a".repeat(500));
    let err = check_destructive_argv(&[long]).expect_err("must refuse");
    assert!(err.len() < 600, "{} chars", err.len());
    assert!(!err.contains(['\n', '\u{1b}']), "{err:?}");
    assert!(err.contains("written as a path"), "{err}");
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
    // The bare form passes this check (a session still cannot run tole).
    assert!(check_destructive_argv(&s(&["mytool", "list"])).is_ok());
    assert!(check_destructive_argv(&s(&["bash", "-c", "mytool list | head -20"])).is_ok());
}

#[test]
fn run_command_gives_the_same_reasons() {
    let t = RunCommandTool::new(std::env::temp_dir());
    let run = |c: &str| t.execute(json!({ "command": c })).unwrap_err();
    let err = run("./target/release/mytool list");
    assert!(
        err.contains("written as a path") && err.contains("`mytool`"),
        "{err}"
    );
    assert!(run("/bin/rm -rf /").contains(DESTRUCTIVE));
    assert!(run("bash -c '/bin/rm -rf /'").contains(DESTRUCTIVE));
}

/// A session reads one consistent story about the tole binary: the path
/// form and the bare form are both refused, and neither advice contradicts
/// the other.
#[test]
fn run_command_with_tole_says_the_same_thing_for_the_path_and_the_bare_form() {
    let t = RunCommandTool::new(std::env::temp_dir());
    let run = |c: &str| t.execute(json!({ "command": c })).unwrap_err();
    let path = run("./tole discuss list");
    let bare = run("tole discuss list");
    for err in [&path, &bare] {
        assert!(err.contains("may not run the tole binary"), "{err}");
        assert!(!err.contains("use the bare name"), "{err}");
    }
    assert!(path.contains("written as a path"), "{path}");
}
