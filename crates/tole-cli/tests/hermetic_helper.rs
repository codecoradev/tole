//! #993: the shared hermetic helper (`tests/common/hermetic.rs`) does what
//! its doc says: no `cora` or `uteke` directory on the child's `PATH`, the
//! test home as `HOME` and `CODECORA_HOME`, no XDG override, and the test's
//! own extra environment applied last.

mod common;
use common::{
    apply_hermetic_env, hermetic_command, path_without_sibling_binaries_from, write_executable,
    TestDir,
};
use std::ffi::OsString;
use std::process::Command;

fn dir_with(root: &TestDir, name: &str, executables: &[&str]) -> std::path::PathBuf {
    let dir = root.join(name);
    std::fs::create_dir_all(&dir).unwrap();
    for exe in executables {
        write_executable(dir.join(exe), "#!/bin/sh\nexit 0\n");
    }
    dir
}

#[test]
fn a_directory_holding_cora_or_uteke_is_dropped_from_the_path_and_the_rest_is_kept() {
    let root = TestDir::new("tole-hermetic-helper-path");
    let with_cora = dir_with(&root, "a", &["cora"]);
    let with_uteke = dir_with(&root, "b", &["uteke", "ls"]);
    let plain = dir_with(&root, "c", &["git"]);
    let missing = root.join("does-not-exist");
    let path = std::env::join_paths([&with_cora, &plain, &with_uteke, &missing]).unwrap();
    let kept: Vec<_> = std::env::split_paths(&path_without_sibling_binaries_from(&path)).collect();
    // A directory with `uteke` goes even though it holds other tools too.
    assert_eq!(kept, vec![plain, missing]);
}

#[test]
fn an_empty_or_unset_path_stays_empty() {
    assert_eq!(
        path_without_sibling_binaries_from(&OsString::new()),
        OsString::new()
    );
}

/// The child sees the test home and not the developer's.
#[test]
fn the_child_gets_the_test_home_and_no_xdg_override() {
    let root = TestDir::new("tole-hermetic-helper-env");
    let home = root.join("home");
    let mut cmd = Command::new("/bin/sh");
    cmd.args([
        "-c",
        "printf '%s|%s|%s' \"$HOME\" \"$CODECORA_HOME\" \"${XDG_CONFIG_HOME-unset}\"",
    ])
    // A leak that the helper must remove.
    .env("XDG_CONFIG_HOME", "/developers/config");
    apply_hermetic_env(&mut cmd, &home, &[]);
    let out = cmd.output().unwrap();
    assert!(out.status.success());
    let h = home.display();
    assert_eq!(
        String::from_utf8_lossy(&out.stdout),
        format!("{h}|{h}|unset")
    );
    assert!(home.is_dir(), "the helper creates the test home");
}

#[test]
fn the_tests_own_extra_environment_is_applied_last() {
    let root = TestDir::new("tole-hermetic-helper-extra");
    let home = root.join("home");
    let other = root.join("other-home");
    let mut cmd = hermetic_command(
        "/bin/sh",
        &home,
        &[("HOME", other.to_str().unwrap()), ("PATH", "/decoy:/bin")],
    );
    cmd.args(["-c", "printf '%s|%s' \"$HOME\" \"$PATH\""]);
    let out = cmd.output().unwrap();
    assert_eq!(
        String::from_utf8_lossy(&out.stdout),
        format!("{}|/decoy:/bin", other.display())
    );
}
