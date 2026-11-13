//! Process-level hardening against same-user inspection (#675).
//!
//! The ENV-1 scrub ([`crate::subprocess::scrub_env_for`]) edits only the
//! environment a CHILD starts with. On Linux a child running as the same
//! user could still read its parent's `/proc/<ppid>/environ` (the
//! environment tole was started with: provider key, `GITHUB_TOKEN`, webhook
//! keys) and `/proc/<ppid>/mem`, or attach with ptrace. [`deny_inspection`]
//! marks the process non-dumpable (`prctl(PR_SET_DUMPABLE, 0)`): the kernel
//! then gives those files to root and refuses same-user ptrace. Side
//! effects: no core dumps of tole, and a debugger needs root to attach.
//!
//! The flag belongs to this process only. The kernel resets it to 1 in an
//! exec'd child, which is fine: the parent's files are what must stay
//! closed. Every `tole` command (serve, mission, cron, child agents, which
//! are `tole run` processes) calls it first thing in `main`, before any
//! thread or child exists.
//!
//! Not done: removing secret-shaped variables from tole's own environment.
//! It would not help (`/proc/<pid>/environ` shows the exec-time block, which
//! `unsetenv` does not erase), tole reads several secrets lazily and passes
//! granted ones to children by inheritance, and changing the environment of
//! a process that may already run threads is unsound.
//!
//! Residual, outside this control: other systems (macOS `KERN_PROCARGS2`,
//! Windows process memory) expose a same-user process's environment by
//! other means, and anything the owner stores in a readable file stays
//! readable; both belong to the sandbox work (#601).

use std::io;

/// Deny same-user inspection of this process (Linux: non-dumpable). A no-op
/// returning `Ok` where the platform has no such switch.
pub fn deny_inspection() -> io::Result<()> {
    #[cfg(target_os = "linux")]
    {
        // SAFETY: PR_SET_DUMPABLE takes an integer argument and touches no
        // memory of ours.
        let rc = unsafe { libc::prctl(libc::PR_SET_DUMPABLE, 0, 0, 0, 0) };
        if rc != 0 {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(())
}

/// Whether same-user inspection of this process is denied: `Some(true)`
/// when it is non-dumpable, `None` where the platform cannot tell.
pub fn inspection_denied() -> Option<bool> {
    #[cfg(target_os = "linux")]
    {
        // SAFETY: PR_GET_DUMPABLE takes no pointer.
        let rc = unsafe { libc::prctl(libc::PR_GET_DUMPABLE, 0, 0, 0, 0) };
        if rc >= 0 {
            return Some(rc == 0);
        }
    }
    None
}
