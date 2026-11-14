//! #601: a sandboxed server idle past `[sandbox] idle_secs` is stopped, even
//! though its session lives; the session's next call starts it again.
//!
//! Needs a working `bwrap`; without one it prints `SKIPPED:` with the
//! reason (`TOLE_REQUIRE_BWRAP=1` makes that a failure).

#![cfg(unix)]

mod support;

use support::{live_policy, register, running, skipped, wait_until, workspace};
use tole_core::mcp::{get_server_prompt_in, live_connections};

#[test]
fn an_idle_server_is_reaped_and_restarts_on_the_next_call() {
    live_policy(8, 2);
    if skipped() {
        return;
    }
    let ws = workspace("idle");
    let (_cfg, _reg) = register("idle", &ws);
    assert!(running(&ws) && live_connections() == 1);
    assert!(
        wait_until(|| !running(&ws) && live_connections() == 0),
        "unused for idle_secs, the server is stopped"
    );
    // The session is still alive: its next call starts the server again.
    let args = [("topic".to_string(), "x".to_string())].into();
    assert_eq!(
        get_server_prompt_in("idle", Some(&ws), "review", args).unwrap(),
        "Review: x"
    );
    assert!(running(&ws) && live_connections() == 1, "restarted");
}
