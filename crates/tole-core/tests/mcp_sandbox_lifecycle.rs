//! #601: the life of sandboxed MCP servers. A session's claim on a server is
//! held by its tool registry and released when it drops; the last claim
//! stops the server and its process is gone. A cap on live sandboxed
//! servers refuses the launch past it (it never evicts another session's
//! server) and a released slot is reusable.
//!
//! Needs a working `bwrap`; without one it prints `SKIPPED:` with the
//! reason (`TOLE_REQUIRE_BWRAP=1` makes that a failure). The idle timeout
//! has its own process in `mcp_sandbox_idle.rs`.

#![cfg(unix)]

mod support;

use std::path::PathBuf;
use support::{live_policy, register, running, skipped, wait_until, workspace};
use tole_core::mcp::{get_server_prompt_in, live_connections};

const CAP: usize = 3;

#[test]
fn a_released_session_stops_its_server_and_the_cap_refuses_the_next() {
    live_policy(CAP, 86_400);
    if skipped() {
        return;
    }
    let args = || [("topic".to_string(), "x".to_string())].into();

    // 1. One session: one server, running.
    let wa = workspace("la");
    let (_ca, reg_a) = register("life", &wa);
    assert_eq!(live_connections(), 1);
    assert!(running(&wa), "the server process runs");

    // 2. Two sessions with the same key share it; it ends with the last.
    let (_ca2, reg_a2) = register("life", &wa);
    assert_eq!(live_connections(), 1, "one server for one key");
    drop(reg_a);
    std::thread::sleep(std::time::Duration::from_millis(600));
    assert!(running(&wa) && live_connections() == 1, "still claimed");
    drop(reg_a2);
    assert!(
        wait_until(|| !running(&wa) && live_connections() == 0),
        "the last release stops the server and the process is gone"
    );
    let e = get_server_prompt_in("life", Some(&wa), "review", args()).unwrap_err();
    assert!(e.contains("never registered"), "its config went too: {e}");

    // 3. The cap: CAP servers run, the next launch is refused, nothing is
    // evicted, and a released slot is reusable.
    let wss: Vec<PathBuf> = (1..=CAP + 1)
        .map(|i| workspace(&format!("lc{i}")))
        .collect();
    let mut held: Vec<_> = wss[..CAP].iter().map(|w| register("cap", w)).collect();
    assert_eq!(live_connections(), CAP);
    let (_c4, reg4) = register("cap", &wss[CAP]);
    assert!(
        !running(&wss[CAP]),
        "the {}th server was not started",
        CAP + 1
    );
    assert_eq!(live_connections(), CAP, "nothing was evicted for it");
    for w in &wss[..CAP] {
        assert!(running(w), "an existing session's server kept running");
    }
    drop(reg4);
    let (_c1, reg1) = held.remove(0);
    drop(reg1);
    assert!(wait_until(|| live_connections() == CAP - 1), "slot freed");
    let (_c5, _reg5) = register("cap", &wss[CAP]);
    assert!(
        running(&wss[CAP]) && live_connections() == CAP,
        "slot reused"
    );
}
