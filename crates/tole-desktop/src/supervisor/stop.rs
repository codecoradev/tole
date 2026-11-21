//! Stopping the serve: ask politely, wait for proof, then insist.
//!
//! The proof that a serve is gone is its lock: `tole serve` holds an exclusive
//! `flock` on `<socket>.lock` for as long as it lives, and the kernel drops it
//! when the process ends however it ends. When the desktop can take that lock,
//! no serve is running, and it may clean up behind it (a socket file a signal
//! left, the credential file).

use std::time::{Duration, Instant};

use super::engine::Engine;
use super::probe::{self, Probe};
use super::sys;

/// Why a serve could not be stopped.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StopError {
    /// It is running, and the desktop does not know its process (it was not
    /// started here and the system does not say who listens on the socket).
    UnknownProcess,
    /// The signal could not be sent.
    Signal(String),
    /// It was still running after `SIGTERM` and `SIGKILL`.
    StillRunning,
}

impl Engine {
    /// Stop the serve on the socket, whoever started it, and remove what it
    /// leaves behind. Returns when its lock is free, or says why not.
    pub fn stop_serve(&mut self) -> Result<(), StopError> {
        self.retry_at = None;
        let socket = self.cfg.socket();
        let lock = self.cfg.serve_lock();
        if !self.serve_is_gone(&lock) {
            let pid = self
                .owned
                .as_ref()
                .map(|o| o.serve.pid())
                .or_else(|| sys::socket_owner_pid(&socket))
                .ok_or(StopError::UnknownProcess)?;
            // A process that is already gone (ESRCH) is what we want.
            if let Err(e) = sys::terminate(pid) {
                if e.raw_os_error() != Some(libc::ESRCH) {
                    return Err(StopError::Signal(e.to_string()));
                }
            }
            if !self.wait_until_gone(&lock, self.cfg.stop_timeout) {
                // It ignored the request: now it is stopped without being asked.
                let _ = sys::kill(pid);
                if !self.wait_until_gone(&lock, Duration::from_secs(2)) {
                    return Err(StopError::StillRunning);
                }
            }
        }
        self.reap_child();
        self.clean_up(&lock);
        self.hub.set_credential(None);
        Ok(())
    }

    fn serve_is_gone(&self, lock: &std::path::Path) -> bool {
        sys::serve_lock_is_free(lock).unwrap_or(false)
    }

    fn wait_until_gone(&self, lock: &std::path::Path, wait: Duration) -> bool {
        let deadline = Instant::now() + wait;
        loop {
            if self.serve_is_gone(lock) {
                return true;
            }
            if Instant::now() >= deadline {
                return false;
            }
            std::thread::sleep(Duration::from_millis(25));
        }
    }

    /// The child has exited by now (its lock is free); collect it so it does
    /// not stay a zombie. One that somehow still runs is killed.
    fn reap_child(&mut self) {
        let Some(mut owned) = self.owned.take() else {
            return;
        };
        let deadline = Instant::now() + Duration::from_secs(2);
        while Instant::now() < deadline {
            if matches!(owned.serve.try_wait(), Ok(Some(_))) {
                return;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        owned.serve.kill();
    }

    /// Behind a serve that is gone: the socket file a signal left, and the
    /// credential. Done while holding its lock, so a serve that starts at this
    /// moment cannot have its fresh socket removed.
    fn clean_up(&self, lock: &std::path::Path) {
        use std::os::unix::fs::FileTypeExt;
        let guard = sys::take_serve_lock(lock);
        let socket = self.cfg.socket();
        let nothing_listens = matches!(probe::probe(&socket), Probe::Absent);
        if matches!(guard, Ok(Some(_))) && nothing_listens {
            let is_socket =
                std::fs::symlink_metadata(&socket).is_ok_and(|m| m.file_type().is_socket());
            if is_socket {
                let _ = std::fs::remove_file(&socket);
            }
            super::token::remove(&self.cfg.runtime_dir);
        }
    }
}
