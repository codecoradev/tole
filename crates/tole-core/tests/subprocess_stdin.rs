//! Unit test of `run_with_timeout_stdin`, moved out of `src/` (#1249): it only uses the public
//! API of the crate.
#![cfg(feature = "shell-tools")]

use std::process::Command;
use tole_core::subprocess::{run_with_timeout_stdin, SUBPROCESS_TIMEOUT};

#[test]
fn stdin_helper_feeds_data_and_drains_output() {
    // Large output + stdin data: proves no pipe deadlock and that
    // the child actually received the bytes.
    let mut cmd = Command::new("sh");
    cmd.arg("-c")
        .arg("cat > /tmp/tole-stdin-test; head -c 200000 /dev/zero | tr '\\0' 'x'");
    let out = run_with_timeout_stdin(&mut cmd, SUBPROCESS_TIMEOUT, b"hello stdin").unwrap();
    assert!(out.status.success());
    assert_eq!(
        std::fs::read_to_string("/tmp/tole-stdin-test").unwrap(),
        "hello stdin"
    );
    assert_eq!(out.stdout.len(), 200_000);
}
