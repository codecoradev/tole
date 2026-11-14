//! #600: probing a server that is not approved. The probe lists its tools
//! and leaves nothing behind: the server and everything it started are
//! gone on every exit path (success, failure, timeout), it receives no
//! secret, and it is not started at all where the sandbox is required and
//! does not apply. One test binary, one process: it sets an environment
//! variable and never sets a sandbox policy.

#![cfg(target_os = "linux")]

#[path = "support/test_dir.rs"]
mod test_dir;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use test_dir::TestDir;
use tole_core::mcp::{probe_manifest, probe_manifest_within, McpServerConfig};

const SECRET: &str = "probe-secret-0123456789abcdef";

fn dir(tag: &str) -> TestDir {
    TestDir::new(&format!("tole-probe-{tag}"))
}

fn cfg(d: &Path, mode: &str) -> McpServerConfig {
    let script = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/probe_mcp.py");
    McpServerConfig::parse(&format!("probe=python3 {script} {mode} {}", d.display())).unwrap()
}

fn pid(d: &Path, file: &str) -> i32 {
    let until = Instant::now() + Duration::from_secs(5);
    loop {
        if let Ok(s) = std::fs::read_to_string(d.join(file)) {
            if let Ok(p) = s.trim().parse() {
                return p;
            }
        }
        assert!(Instant::now() < until, "{file} was never written");
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// Gone: no such process, or a zombie awaiting its reaper.
fn gone(pid: i32) -> bool {
    match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
        Err(_) => true,
        Ok(s) => s
            .rsplit(')')
            .next()
            .is_some_and(|rest| rest.trim_start().starts_with('Z')),
    }
}

fn assert_all_gone(d: &Path) {
    let (server, grandchild) = (pid(d, "server"), pid(d, "grandchild"));
    let until = Instant::now() + Duration::from_secs(5);
    while !(gone(server) && gone(grandchild)) {
        assert!(
            Instant::now() < until,
            "left running: server {server} gone={} grandchild {grandchild} gone={}",
            gone(server),
            gone(grandchild)
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn a_probe_lists_the_tools_and_leaves_no_process_behind() {
    // SAFETY (edition 2021: safe call): the only test of this binary that
    // reads it, and no other thread of this binary sets variables.
    std::env::set_var("MY_PROBE_API_KEY", SECRET);

    // Success: the tools come back, the server and its grandchild are gone,
    // and the secret never reached it (the grant of an unapproved server is
    // empty, whatever the release says).
    let d = dir("ok");
    let (version, tools) = probe_manifest(&cfg(&d, "ok"), false).unwrap();
    assert_eq!(version, "1.2.3");
    assert_eq!(tools.len(), 1);
    assert_eq!(tools[0].name, "add");
    assert_eq!(tools[0].description, "Add two numbers.");
    assert_all_gone(&d);
    let env = std::fs::read_to_string(d.join("env")).unwrap();
    assert!(!env.contains(SECRET), "a secret reached the probed server");
    assert!(!env.contains("MY_PROBE_API_KEY"));

    // A server that dies at once: an error, and the grandchild it left (it
    // holds the pipes open, so the probe waits for its budget) is killed
    // with the group.
    let d = dir("die");
    let e = probe_manifest_within(&cfg(&d, "die"), false, Duration::from_secs(3)).unwrap_err();
    assert!(
        e.contains("handshake failed") || e.contains("timed out"),
        "{e}"
    );
    assert_all_gone(&d);

    // A server that never answers: the probe times out and ends the group.
    let d = dir("hang");
    let t = Instant::now();
    let e = probe_manifest_within(&cfg(&d, "hang"), false, Duration::from_secs(2)).unwrap_err();
    assert!(e.contains("timed out"), "{e}");
    assert!(t.elapsed() < Duration::from_secs(20), "bounded");
    assert_all_gone(&d);

    // Sandbox required and none configured: refused before anything runs.
    let d = dir("refused");
    let e = probe_manifest(&cfg(&d, "ok"), true).unwrap_err();
    assert!(e.contains("must run in the sandbox"), "{e}");
    assert!(!d.join("server").exists(), "the server was started");
}
