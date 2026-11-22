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

/// The threads started, the ones already joined, and whether `join` began.
#[derive(Default)]
struct Set {
    closed: bool,
    live: Vec<(JoinHandle<()>, Arc<AtomicBool>)>,
    /// Ended threads joined early (see [`Spawner::spawn`]), and how many of
    /// them panicked: `join` still counts them.
    reaped: usize,
    reaped_panics: usize,
}

type Handles = Arc<Mutex<Set>>;

/// Starts threads that [`Threads::join`] will account for, from a thread that
/// is itself one of them (a worker that replaces a lane whose call it
/// abandoned). Cheap to clone.
#[derive(Clone)]
pub struct Spawner {
    handles: Handles,
    exits: Arc<Exits>,
}

impl Spawner {
    /// Start a named thread. `Err` when the system cannot make one, and once
    /// [`Threads::join`] has begun: it counts the threads it finds then, so a
    /// thread started after that would be counted by nobody.
    pub fn spawn(&self, name: &str, body: impl FnOnce() + Send + 'static) -> io::Result<()> {
        let mut set = self.handles.lock().unwrap_or_else(|p| p.into_inner());
        if set.closed {
            return Err(io::Error::other("the driver is stopping"));
        }
        // A thread that ended is joined now, not held until `join`: a long
        // driver that abandons calls would keep one finished thread each.
        let (ended, live): (Vec<_>, Vec<_>) = std::mem::take(&mut set.live)
            .into_iter()
            .partition(|(_, flag)| flag.load(Ordering::SeqCst));
        set.live = live;
        for (handle, _) in ended {
            set.reaped += 1;
            set.reaped_panics += usize::from(handle.join().is_err());
        }
        let flag = Arc::new(AtomicBool::new(false));
        let guard = ExitGuard(Arc::clone(&self.exits), Arc::clone(&flag));
        let handle = std::thread::Builder::new()
            .name(name.to_owned())
            .spawn(move || {
                let _guard = guard;
                body();
            })?;
        set.live.push((handle, flag));
        Ok(())
    }
}

/// The threads of one driver.
#[derive(Default)]
pub struct Threads {
    handles: Handles,
    exits: Arc<Exits>,
}

impl Threads {
    pub fn new() -> Self {
        Self::default()
    }

    /// A handle that starts threads of this set from other threads.
    pub fn spawner(&self) -> Spawner {
        Spawner {
            handles: Arc::clone(&self.handles),
            exits: Arc::clone(&self.exits),
        }
    }

    /// Start a named thread. `Err` only when the system cannot make one.
    pub fn spawn(&mut self, name: &str, body: impl FnOnce() + Send + 'static) -> io::Result<()> {
        self.spawner().spawn(name, body)
    }

    /// Wait up to `timeout` for every thread to end, join the ones that did and
    /// let go of the rest. Raise the driver's stop signal first.
    pub fn join(&mut self, timeout: Duration) -> JoinReport {
        let (taken, reaped, reaped_panics) = {
            let mut set = self.handles.lock().unwrap_or_else(|p| p.into_inner());
            set.closed = true;
            (
                std::mem::take(&mut set.live),
                std::mem::take(&mut set.reaped),
                std::mem::take(&mut set.reaped_panics),
            )
        };
        let total = taken.len() + reaped;
        {
            let done = self.exits.done.lock().unwrap_or_else(|p| p.into_inner());
            let _ = self
                .exits
                .changed
                .wait_timeout_while(done, timeout, |n| *n < total);
        }
        let mut report = JoinReport {
            joined: reaped,
            panicked: reaped_panics,
            detached: 0,
        };
        for (handle, ended) in taken {
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
