//! The launch: the supervisor is started off the window's thread, and the
//! backend follows it through [`LaunchLink`] from the first moment.
//!
//! `SupervisorLink::launch` blocks (up to the start-up timeout plus the wait
//! for another launch of the desktop), so the window cannot wait for it. The
//! backend needs a link at once, so this one stands in until the supervisor
//! exists:
//!
//! | phase | the link says |
//! |---|---|
//! | launching | `Starting` |
//! | up | the supervisor's own state, epoch and `Api` |
//! | failed (the launch panicked, or no thread could be started) | `Failed(StartFailed { detail })` |
//! | closed (the window is closing) | `Stopped` |
//!
//! The epoch is the number of phase changes in its high half and the
//! supervisor's own epoch in the low half, so a new launch reads as a new
//! serve to the backend, which then builds new drivers over a new `Api`.
//!
//! A launch that ends after the window closed is not kept: its serve gets
//! `close_window` on the launch thread, which nobody waits for.

use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread;

use tole_desktop_model::{strings, ServeFailure, ServeState};

use super::signal::Signal;
use crate::backend::{ServeLink, SupervisorLink};
use crate::driver::api::Api;
use crate::supervisor::Config;

/// A launched serve: what the backend reads ([`ServeLink`]) and what the
/// window does to it. [`SupervisorLink`] is the real one; tests script their
/// own.
pub trait Serve: ServeLink {
    /// The window is closing: what the settings say happens to the serve (by
    /// default it keeps running). Consumes the handle and says what it did.
    fn close_window(self: Arc<Self>) -> CloseOutcome;
}

/// What [`Serve::close_window`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CloseOutcome {
    /// The serve was closed as the settings say.
    Closed,
    /// Something else still held the serve (a backend thread that was let
    /// go), so it was only released and left running: a setting that would
    /// have stopped it was not carried out.
    Held,
    /// The supervisor could not stop the serve.
    Failed,
}

impl Serve for SupervisorLink {
    fn close_window(self: Arc<Self>) -> CloseOutcome {
        let Ok(link) = Arc::try_unwrap(self) else {
            return CloseOutcome::Held;
        };
        let Ok(supervisor) = Arc::try_unwrap(link.into_supervisor()) else {
            return CloseOutcome::Held;
        };
        match supervisor.close_window() {
            Ok(()) => CloseOutcome::Closed,
            Err(_) => CloseOutcome::Failed,
        }
    }
}

/// Starts a serve: blocks until the launch is settled. Called again for a
/// retry after a launch that failed.
pub type Launcher = Arc<dyn Fn() -> Arc<dyn Serve> + Send + Sync + 'static>;

/// The launcher of the real supervisor for `cfg`.
pub fn supervisor_launcher(cfg: Config) -> Launcher {
    Arc::new(move || {
        let link: Arc<dyn Serve> = Arc::new(SupervisorLink::launch(cfg.clone()));
        link
    })
}

#[derive(Clone)]
enum Phase {
    Launching,
    Up(Arc<dyn Serve>),
    Failed(String),
    Closed,
}

/// What closing found.
pub struct Closing {
    /// The serve, if the launch had finished.
    pub serve: Option<Arc<dyn Serve>>,
    /// The launch was still running: it is let go (see the module docs).
    pub launching: bool,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    // Nothing behind these locks has an invariant a panic could break.
    m.lock().unwrap_or_else(|p| p.into_inner())
}

fn panic_text(payload: &(dyn std::any::Any + Send)) -> String {
    payload
        .downcast_ref::<&str>()
        .map(|s| (*s).to_owned())
        .or_else(|| payload.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "no message".to_owned())
}

/// The serve as the backend sees it, from the first moment (see the module
/// docs).
pub struct LaunchLink {
    phase: Mutex<Phase>,
    /// How many times the phase changed.
    changes: AtomicU64,
    launcher: Launcher,
    signal: Signal,
}

impl LaunchLink {
    /// A link that reads `Starting` and has launched nothing yet. The
    /// backend is started over it, and only then [`begin`](Self::begin) starts
    /// the launch: a backend that cannot start must leave no serve behind.
    pub fn new(launcher: Launcher, signal: Signal) -> Arc<LaunchLink> {
        Arc::new(LaunchLink {
            phase: Mutex::new(Phase::Launching),
            changes: AtomicU64::new(0),
            launcher,
            signal,
        })
    }

    /// Start launching on a thread of its own and return at once. Once, after
    /// [`new`](Self::new).
    pub fn begin(self: &Arc<Self>) {
        self.spawn_launch();
    }

    fn current(&self) -> Phase {
        lock(&self.phase).clone()
    }

    /// Count a change of phase. Called with the phase lock held, so the count
    /// and the phase move together: a reader that sees the new phase sees the
    /// new count too.
    fn count_change(&self) {
        self.changes.fetch_add(1, Ordering::SeqCst);
    }

    fn spawn_launch(self: &Arc<Self>) {
        let me = Arc::clone(self);
        let spawned = thread::Builder::new()
            .name("tole-desktop-launch".into())
            .spawn(move || me.launch());
        if let Err(e) = spawned {
            self.settle(Err(strings::launch_thread_failed(&e.to_string())));
        }
    }

    /// The launch thread: a panic is a failed launch, never a lost one.
    fn launch(&self) {
        let outcome = catch_unwind(AssertUnwindSafe(|| (self.launcher)()))
            .map_err(|p| strings::launch_panicked(&panic_text(&*p)));
        self.settle(outcome);
    }

    fn settle(&self, outcome: Result<Arc<dyn Serve>, String>) {
        let late = {
            let mut phase = lock(&self.phase);
            if matches!(*phase, Phase::Launching) {
                *phase = match outcome {
                    Ok(serve) => Phase::Up(serve),
                    Err(detail) => Phase::Failed(detail),
                };
                self.count_change();
                None
            } else {
                // The window closed while this ran.
                outcome.ok()
            }
        };
        self.signal.raise();
        if let Some(serve) = late {
            // Nobody is left to show how it went.
            let _ = serve.close_window();
        }
    }

    /// The window is closing: from now on the link reads `Stopped`, a launch
    /// that ends later is closed on its own thread, and the serve, if there is
    /// one, is the caller's to close.
    pub fn close(&self) -> Closing {
        let was = {
            let mut phase = lock(&self.phase);
            self.count_change();
            std::mem::replace(&mut *phase, Phase::Closed)
        };
        self.signal.raise();
        match was {
            Phase::Up(serve) => Closing {
                serve: Some(serve),
                launching: false,
            },
            Phase::Launching => Closing {
                serve: None,
                launching: true,
            },
            Phase::Failed(_) | Phase::Closed => Closing {
                serve: None,
                launching: false,
            },
        }
    }
}

impl ServeLink for LaunchLink {
    fn state(&self) -> ServeState {
        match self.current() {
            Phase::Launching => ServeState::Starting,
            Phase::Up(serve) => serve.state(),
            Phase::Failed(detail) => ServeState::Failed(ServeFailure::StartFailed { detail }),
            Phase::Closed => ServeState::Stopped,
        }
    }

    fn epoch(&self) -> u64 {
        let changes = self.changes.load(Ordering::SeqCst) << 32;
        match self.current() {
            Phase::Up(serve) => changes | (serve.epoch() & 0xffff_ffff),
            _ => changes,
        }
    }

    fn api(&self) -> Option<Arc<dyn Api>> {
        match self.current() {
            Phase::Up(serve) => serve.api(),
            _ => None,
        }
    }
}
