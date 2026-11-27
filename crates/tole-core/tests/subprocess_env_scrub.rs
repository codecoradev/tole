//! Unit test of `scrub_env_for_child`, moved out of `src/` (#1249): it only uses the public
//! API of the crate.
#![cfg(feature = "shell-tools")]

use std::process::Command;
use tole_core::subprocess::{run_with_timeout, scrub_env_for_child, SUBPROCESS_TIMEOUT};

/// The scrubber mutates the Command; verify via a child that prints
/// its own env, run through run_with_timeout.
fn child_env(cmd: &mut Command) -> String {
    let out = run_with_timeout(cmd, SUBPROCESS_TIMEOUT).unwrap();
    assert!(out.status.success());
    String::from_utf8_lossy(&out.stdout).to_string()
}

#[test]
fn secret_shaped_env_removed_but_normal_kept() {
    std::env::set_var("TOLE_TEST_API_KEY", "x");
    std::env::set_var("TOLE_TEST_SECRET_VALUE", "x");
    std::env::set_var("MY_PASSWORD", "x");
    // #284: names without the underscore-prefixed form.
    std::env::set_var("SECRET_KEY", "x");
    std::env::set_var("TOLE_TEST_AUTHTOKEN", "x");
    std::env::set_var("TOLE_TEST_DB_PASSWD", "x");
    std::env::set_var("TOLE_TEST_APIKEY", "x");
    std::env::set_var("TOLE_TEST_KEEP_ME", "visible");
    let mut cmd = Command::new("sh");
    cmd.arg("-c").arg("env");
    let env = child_env(scrub_env_for_child(&mut cmd));
    assert!(!env.contains("TOLE_TEST_API_KEY"));
    assert!(!env.contains("TOLE_TEST_SECRET_VALUE"));
    assert!(!env.contains("MY_PASSWORD"));
    assert!(!env.contains("SECRET_KEY="));
    assert!(!env.contains("TOLE_TEST_AUTHTOKEN"));
    assert!(!env.contains("TOLE_TEST_DB_PASSWD"));
    assert!(!env.contains("TOLE_TEST_APIKEY"));
    assert!(env.contains("TOLE_TEST_KEEP_ME=visible"));
    assert!(env.contains("PATH="));
    std::env::remove_var("TOLE_TEST_API_KEY");
    std::env::remove_var("TOLE_TEST_SECRET_VALUE");
    std::env::remove_var("MY_PASSWORD");
    std::env::remove_var("SECRET_KEY");
    std::env::remove_var("TOLE_TEST_AUTHTOKEN");
    std::env::remove_var("TOLE_TEST_DB_PASSWD");
    std::env::remove_var("TOLE_TEST_APIKEY");
    std::env::remove_var("TOLE_TEST_KEEP_ME");
}
