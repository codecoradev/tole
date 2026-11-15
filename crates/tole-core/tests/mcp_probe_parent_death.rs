//! #848: the probe's server is in its own process group, so a terminal Ctrl-C
//! no longer reaches it, and tole has no signal handler. If tole dies in the
//! middle of a probe, the server must not outlive it. The test runs the probe
//! in a helper process (this test binary again), kills the helper while the
//! server waits for its handshake, and expects the server to be gone.

#![cfg(target_os = "linux")]

#[path = "support/test_dir.rs"]
mod test_dir;
use std::process::{Command, Stdio};
use test_dir::{wait_until, TestDir, WAIT};
use tole_core::mcp::{probe_manifest, McpServerConfig};

const HELPER: &str = "TOLE_PROBE_PARENT_DEATH_SPEC";

/// `true` once `pid` is gone or only a zombie (not yet reaped by init).
fn gone(pid: i32) -> bool {
    match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
        Err(_) => true,
        Ok(s) => s.rsplit(") ").next().is_some_and(|r| r.starts_with('Z')),
    }
}

#[test]
fn the_probe_server_does_not_outlive_its_parent() {
    // Helper mode: probe a server that never answers, until killed.
    if let Ok(spec) = std::env::var(HELPER) {
        let cfg = McpServerConfig::parse(&spec).unwrap();
        let _ = probe_manifest(&cfg, false);
        return;
    }
    let dir = TestDir::new("tole-probe-parent-death");
    let script = dir.join("server.sh");
    test_dir::write_executable(
        &script,
        format!(
            "#!/bin/sh\necho $$ > {d}/pid\nexec sleep 600\n",
            d = dir.display()
        ),
    );
    let mut helper = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "the_probe_server_does_not_outlive_its_parent",
            "--nocapture",
        ])
        .env(HELPER, format!("hang={}", script.display()))
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let pid_file = dir.join("pid");
    wait_until(WAIT, "the probe server to start", || {
        std::fs::read_to_string(&pid_file).is_ok_and(|p| p.trim().parse::<i32>().is_ok())
    });
    let pid: i32 = std::fs::read_to_string(&pid_file)
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    // Never leak the sleeper, whatever the verdict.
    let _reap = KillOnDrop(pid);
    assert!(!gone(pid), "the server is running mid-probe");
    // The parent dies without running any Drop (as on Ctrl-C).
    helper.kill().unwrap();
    let _ = helper.wait();
    wait_until(WAIT, "the probe server to die with its parent", || {
        gone(pid)
    });
}

struct KillOnDrop(i32);

impl Drop for KillOnDrop {
    fn drop(&mut self) {
        // SAFETY: a plain kill(2) of a pid this test started.
        unsafe {
            libc::kill(self.0, libc::SIGKILL);
        }
    }
}
