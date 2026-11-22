//! The threads of a driver, spawned together and stopped together.
//!
//! Shared by every driver. [`Threads::join`] waits for the threads to say they
//! are done (a counter and a condvar, no polling), up to a timeout. A thread
//! that is still inside a blocking call then (a request may take 30 s, an event
//! stream is silent for up to its idle timeout) is not waited for: it is
//! counted in [`JoinReport::detached`], holds nothing the driver still needs,
//! and ends by itself when the call returns.

use std::io;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

/// What [`Threads::join`] found.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct JoinReport {
    /// Threads that ended and were joined.
    pub joined: usize,
    /// Of those, the ones that ended by a panic.
    pub panicked: usize,
    /// Threads still in a blocking call when the timeout ran out.
    pub detached: usize,
}

#[derive(Default)]
struct Exits {
    done: Mutex<usize>,
    changed: Condvar,
}

/// Counts a thread as done when dropped, also when it unwinds from a panic.
struct ExitGuard(Arc<Exits>, Arc<AtomicBool>);

impl Drop for ExitGuard {
    fn drop(&mut self) {
        self.1.store(true, Ordering::SeqCst);
        *self.0.done.lock().unwrap_or_else(|p| p.into_inner()) += 1;
        self.0.changed.notify_all();
    }
}

/// The threads of one driver.
#[derive(Default)]
pub struct Threads {
    handles: Vec<(JoinHandle<()>, Arc<AtomicBool>)>,
    exits: Arc<Exits>,
}

impl Threads {
    pub fn new() -> Self {
        Self::default()
    }

    /// Start a named thread. `Err` only when the system cannot make one.
    pub fn spawn(&mut self, name: &str, body: impl FnOnce() + Send + 'static) -> io::Result<()> {
        let ended = Arc::new(AtomicBool::new(false));
        let guard = ExitGuard(Arc::clone(&self.exits), Arc::clone(&ended));
        let handle = std::thread::Builder::new()
            .name(name.to_owned())
            .spawn(move || {
                let _guard = guard;
                body();
            })?;
        self.handles.push((handle, ended));
        Ok(())
    }

    /// Wait up to `timeout` for every thread to end, join the ones that did and
    /// let go of the rest. Raise the driver's stop signal first.
    pub fn join(&mut self, timeout: Duration) -> JoinReport {
        let total = self.handles.len();
        {
            let done = self.exits.done.lock().unwrap_or_else(|p| p.into_inner());
            let _ = self
                .exits
                .changed
                .wait_timeout_while(done, timeout, |n| *n < total);
        }
        let mut report = JoinReport::default();
        for (handle, ended) in self.handles.drain(..) {
            // The flag is set as the thread's last act: the join is instant.
            if ended.load(Ordering::SeqCst) {
                report.joined += 1;
                if handle.join().is_err() {
                    report.panicked += 1;
                }
            } else {
                report.detached += 1;
            }
        }
        report
    }
}
