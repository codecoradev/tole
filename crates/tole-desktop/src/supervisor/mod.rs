//! The supervisor of the local `tole serve` (issue #1029, owner decisions D1 to
//! D4 in `docs/desktop-design.md` section 0).
//!
//! The desktop is a thin client of `tole serve`, also on the same machine. On
//! launch it attaches to a serve that already listens on its private socket and
//! speaks the protocol it knows, or starts one with a per-launch credential
//! handed over on an inherited descriptor. This module has no window and no
//! toolkit; the window renders the [`tole_desktop_model::ServeState`] it
//! reports.
//!
//! ```no_run
//! use tole_desktop::supervisor::{default_runtime_dir, Config, Supervisor};
//!
//! let runtime = default_runtime_dir(None, &std::env::temp_dir(), 1000);
//! let supervisor = Supervisor::launch(Config::new("/home/me/project", runtime));
//! if let Some(client) = supervisor.client() {
//!     println!("{:?}", client.health());
//! }
//! // Closing the window leaves the serve running (the default);
//! // "quit completely" stops it.
//! let _ = supervisor.quit_completely();
//! ```
//!
//! The rules it keeps:
//!
//! - A second launch attaches and never starts a second serve: the decision is
//!   taken under a launch lock, and the serve's own lock refuses a second
//!   process on the same socket.
//! - A serve that speaks another protocol version is not talked to
//!   ([`tole_desktop_model::ServeFailure::VersionMismatch`]);
//!   [`Supervisor::restart_serve`] replaces it with the binary the desktop
//!   would start.
//! - The socket directory must be the owner's alone, or nothing is sent and
//!   nothing is started there.
//! - A serve that stops by itself is started again with a growing wait; after
//!   too many stops in a row the state says so and waits for the owner.
//! - Dropping a [`Supervisor`] (the window closing) stops watching and leaves
//!   the serve alone; only [`Supervisor::quit_completely`] stops it.
//!
//! macOS and Linux (a Unix socket). The Windows named pipe is a follow-up
//! (#1051).

pub mod binary;
pub mod config;
pub mod engine;
pub mod hub;
pub mod probe;
pub mod process;
pub mod recover;
pub mod stop;
pub mod sys;
pub mod token;

#[cfg(test)]
mod fake;
#[cfg(test)]
mod tests;

use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::thread::JoinHandle;
use std::time::Duration;

use tole_client::Client;

pub use config::{default_runtime_dir, Backoff, CloseBehavior, Config};
pub use hub::Observer;
pub use stop::StopError;
pub use tole_desktop_model::{ServeFailure, ServeState};

use engine::{Engine, Outcome};
use hub::Hub;

/// The local serve, attached or started, and the thread that watches it.
pub struct Supervisor {
    hub: Arc<Hub>,
    engine: Arc<Mutex<Engine>>,
    shutdown: Arc<(Mutex<bool>, Condvar)>,
    monitor: Option<JoinHandle<()>>,
    close_behavior: CloseBehavior,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}

impl Supervisor {
    /// Attach to the serve on the socket or start one, then watch it. Blocks
    /// until that is settled (at most the start-up timeout plus the wait for
    /// another launch), so call it from a worker thread; the result is in
    /// [`Supervisor::state`], and a failure is a state, not a panic.
    pub fn launch(cfg: Config) -> Supervisor {
        Self::launch_inner(cfg, None)
    }

    /// [`Supervisor::launch`], telling `observer` about every state from
    /// `Starting` on. See [`Observer`] for what it may do.
    pub fn launch_observed(
        cfg: Config,
        observer: impl Fn(&ServeState) + Send + Sync + 'static,
    ) -> Supervisor {
        Self::launch_inner(cfg, Some(Box::new(observer)))
    }

    fn launch_inner(cfg: Config, observer: Option<Observer>) -> Supervisor {
        let hub = Hub::new(cfg.socket(), observer);
        let poll = cfg.poll_interval;
        let close_behavior = cfg.close_behavior;
        let mut engine = Engine::new(cfg, Arc::clone(&hub));
        hub.announce(); // Starting
        if let Outcome::Failed(failure) = engine.ensure() {
            hub.set(ServeState::Failed(failure));
        }
        let engine = Arc::new(Mutex::new(engine));
        let shutdown = Arc::new((Mutex::new(false), Condvar::new()));
        let monitor = {
            let (engine, shutdown) = (Arc::clone(&engine), Arc::clone(&shutdown));
            std::thread::Builder::new()
                .name("tole-serve-monitor".into())
                .spawn(move || monitor(&engine, &shutdown, poll))
                .ok()
        };
        Supervisor {
            hub,
            engine,
            shutdown,
            monitor,
            close_behavior,
        }
    }

    pub fn state(&self) -> ServeState {
        self.hub.state()
    }

    /// A client of the serve, when it is ready: attached or running.
    pub fn client(&self) -> Option<Client> {
        self.hub.client()
    }

    /// Try again after a failure: the count of stops starts over.
    pub fn retry(&self) {
        lock(&self.engine).retry_now();
    }

    /// Stop the serve that is there, whoever started it, and start the one
    /// this desktop would start: the answer to a version mismatch or a serve
    /// the desktop cannot sign in to. Blocks until it is done.
    pub fn restart_serve(&self) {
        let mut engine = lock(&self.engine);
        self.hub.set(ServeState::Stopping);
        match engine.stop_serve() {
            Ok(()) => engine.retry_now(),
            Err(_) => self.hub.set(ServeState::Failed(ServeFailure::CouldNotStop)),
        }
    }

    /// "Quit completely": stop watching, stop the serve and what runs in it,
    /// and wait until its lock proves it is gone. On failure the state is
    /// `Failed(CouldNotStop)`.
    pub fn quit_completely(mut self) -> Result<(), StopError> {
        self.stop_monitor();
        let mut engine = lock(&self.engine);
        self.hub.set(ServeState::Stopping);
        match engine.stop_serve() {
            Ok(()) => {
                self.hub.set(ServeState::Stopped);
                Ok(())
            }
            Err(e) => {
                self.hub.set(ServeState::Failed(ServeFailure::CouldNotStop));
                Err(e)
            }
        }
    }

    /// The window is closing: what the Settings say happens to the serve.
    /// By default it keeps running, with its automations and turns.
    pub fn close_window(self) -> Result<(), StopError> {
        match self.close_behavior {
            CloseBehavior::LeaveRunning => Ok(()),
            CloseBehavior::StopServe => self.quit_completely(),
        }
    }

    fn stop_monitor(&mut self) {
        let (flag, wake) = &*self.shutdown;
        *lock(flag) = true;
        wake.notify_all();
        if let Some(handle) = self.monitor.take() {
            let _ = handle.join();
        }
    }
}

impl Drop for Supervisor {
    /// Stops watching. The serve is left running.
    fn drop(&mut self) {
        self.stop_monitor();
    }
}

fn monitor(engine: &Mutex<Engine>, shutdown: &(Mutex<bool>, Condvar), poll: Duration) {
    let (flag, wake) = shutdown;
    let mut wait = poll;
    loop {
        let stopped = lock(flag);
        let (stopped, _) = wake
            .wait_timeout_while(stopped, wait, |stop| !*stop)
            .unwrap_or_else(|p| p.into_inner());
        if *stopped {
            return;
        }
        drop(stopped);
        let hint = lock(engine).tick();
        wait = hint
            .map_or(poll, |d| d.min(poll))
            .max(Duration::from_millis(10));
    }
}
