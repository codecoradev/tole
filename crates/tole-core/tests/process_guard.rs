//! #675: a same-user child must not read its parent's environment through
//! `/proc/<ppid>/environ`. The ENV-1 scrub edits only the child's own
//! environment; `process_guard::deny_inspection` closes the parent's.
//!
//! The secret has to be in the environment the parent was EXEC'd with (that
//! is what `/proc/<pid>/environ` shows), so the test runs itself again as a
//! child with the canary set, and the inner run does the probing.
#![cfg(target_os = "linux")]

use std::process::Command;

const CANARY_VAR: &str = "PROCESS_GUARD_PROBE_API_KEY";
const CANARY: &str = "process-guard-canary-0123456789";

/// What a same-user child prints when it reads its parent's environment.
fn child_reads_parent_environ() -> (bool, String) {
    let out = Command::new("sh")
        .arg("-c")
        .arg("tr '\\0' '\\n' < /proc/$PPID/environ")
        .env_remove(CANARY_VAR)
        .output()
        .expect("spawn sh");
    (
        out.status.success(),
        String::from_utf8_lossy(&out.stdout).into_owned(),
    )
}

/// Runs `test` of this file again in a child whose exec-time environment
/// carries the canary. Root reads every process's files whatever the flag
/// (CAP_SYS_PTRACE), so under root the inner run drops to `nobody`: the
/// guard is about a same-user, unprivileged child.
fn rerun_with_canary(test: &str) {
    use std::os::unix::process::CommandExt;
    let mut cmd = Command::new(std::env::current_exe().unwrap());
    cmd.args([test, "--exact", "--nocapture", "--test-threads=1"])
        .env(CANARY_VAR, CANARY)
        .current_dir("/");
    // SAFETY: geteuid has no preconditions.
    if unsafe { libc::geteuid() } == 0 {
        const NOBODY: libc::uid_t = 65534;
        // SAFETY: only async-signal-safe calls between fork and exec.
        unsafe {
            cmd.pre_exec(|| {
                if libc::setgroups(0, std::ptr::null()) != 0
                    || libc::setgid(NOBODY) != 0
                    || libc::setuid(NOBODY) != 0
                {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
    }
    let out = cmd.output().expect("re-run the test binary");
    assert!(
        out.status.success(),
        "inner run failed:\n{}\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        String::from_utf8_lossy(&out.stdout).contains("1 passed"),
        "the inner test did not run"
    );
}

#[test]
fn a_child_cannot_read_the_parent_environment_once_inspection_is_denied() {
    if std::env::var(CANARY_VAR).as_deref() != Ok(CANARY) {
        return rerun_with_canary(
            "a_child_cannot_read_the_parent_environment_once_inspection_is_denied",
        );
    }
    // Control: before the guard the leak is real here, so the check below
    // means something on this machine.
    let (_, before) = child_reads_parent_environ();
    assert!(
        before.contains(CANARY),
        "control: the child should read the parent's environ before the guard"
    );

    tole_core::process_guard::deny_inspection().expect("prctl");
    assert_eq!(tole_core::process_guard::inspection_denied(), Some(true));

    let (ok, after) = child_reads_parent_environ();
    assert!(!ok, "reading /proc/$PPID/environ must fail");
    assert!(
        !after.contains(CANARY),
        "child read the parent's secret via /proc/$PPID/environ"
    );
}

/// The guard must not break what tole reads about processes: its own
/// `/proc/self` files and another process's `/proc/<pid>/stat` (the
/// delegate lease liveness check reads the start time there). Its own
/// `/proc/self/environ` does become unreadable (root-owned); tole never
/// reads it.
#[test]
fn the_guard_keeps_proc_self_and_stat_readable() {
    if std::env::var(CANARY_VAR).as_deref() != Ok(CANARY) {
        return rerun_with_canary("the_guard_keeps_proc_self_and_stat_readable");
    }
    tole_core::process_guard::deny_inspection().expect("prctl");
    // What tole itself reads: its executable path (`current_exe` is the
    // /proc/self/exe link), its cgroup (resource guard) and stat files.
    assert!(std::env::current_exe().is_ok());
    for f in ["/proc/self/stat", "/proc/self/cgroup"] {
        assert!(std::fs::read_to_string(f).is_ok(), "{f}");
    }
    let out = Command::new("sh")
        .arg("-c")
        .arg("cat /proc/$PPID/stat")
        .output()
        .expect("spawn sh");
    assert!(out.status.success(), "a child reads the parent's stat");
    let stat = String::from_utf8_lossy(&out.stdout);
    assert!(
        stat.starts_with(&format!("{} ", std::process::id())),
        "{stat}"
    );
}
