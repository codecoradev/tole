//! The shared hermetic test environment itself (#993): `common/hermetic.rs`.

mod common;
use common::{hermetic_command, path_without_sibling_binaries_from, write_executable, TestDir};

#[test]
fn path_filter_drops_only_directories_holding_cora_or_uteke() {
    let root = TestDir::new("tole-hermetic-path");
    let with_cora = root.join("a");
    let with_uteke = root.join("b");
    let plain = root.join("c");
    for d in [&with_cora, &with_uteke, &plain] {
        std::fs::create_dir_all(d).unwrap();
    }
    write_executable(with_cora.join("cora"), "#!/bin/sh\n");
    write_executable(with_uteke.join("uteke"), "#!/bin/sh\n");
    write_executable(plain.join("git"), "#!/bin/sh\n");
    let path = std::env::join_paths([&with_cora, &plain, &with_uteke]).unwrap();
    let kept: Vec<_> = std::env::split_paths(&path_without_sibling_binaries_from(&path)).collect();
    assert_eq!(kept, vec![plain]);
}

#[test]
fn command_gets_the_test_home_and_extra_env_wins() {
    let root = TestDir::new("tole-hermetic-env");
    let home = root.join("home");
    let out = hermetic_command(
        "sh",
        &home,
        &[("TOLE_HERMETIC_EXTRA", "yes"), ("CODECORA_HOME", "/decoy")],
    )
    .args([
        "-c",
        "printf '%s|%s|%s|%s' \"$HOME\" \"$CODECORA_HOME\" \"$TOLE_HERMETIC_EXTRA\" \"${XDG_CONFIG_HOME-unset}\"",
    ])
    .output()
    .expect("sh resolves on the filtered PATH");
    assert!(out.status.success());
    let got = String::from_utf8_lossy(&out.stdout).into_owned();
    let want = format!("{}|/decoy|yes|unset", home.display());
    assert_eq!(got, want);
    assert!(home.is_dir(), "the helper creates the home");
}
