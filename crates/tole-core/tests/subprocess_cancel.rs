//! Unit test of `run_with_timeout_or_cancel`, moved out of `src/` (#1249): it only uses the public
//! API of the crate.
#![cfg(all(unix, feature = "shell-tools"))]

use std::process::Command;
use std::time::Duration;
use tole_core::subprocess::{run_with_timeout_or_cancel, CANCELLED};

#[test]
fn a_set_cancel_token_kills_the_child() {
    let cancel = tole_core::cancel::CancelToken::new();
    cancel.cancel();
    let start = std::time::Instant::now();
    let mut cmd = Command::new("sleep");
    cmd.arg("5");
    let r = run_with_timeout_or_cancel(&mut cmd, Duration::from_secs(30), &cancel);
    assert_eq!(r.unwrap_err(), CANCELLED);
    assert!(start.elapsed() < Duration::from_secs(4));
}
