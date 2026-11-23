//! The app: what boots the desktop and what the window reads and calls.
//!
//! No toolkit type is here and no window: [`App`] starts the local serve off
//! the window's thread, keeps the [`Backend`] following it, and gives the
//! window three things: [`App::view`] (plain data), [`App::take_change`] (a
//! change since the last look) and the few commands the window may call
//! ([`App::owner_action`] and [`App::quit`]).
//!
//! ```no_run
//! use std::time::Duration;
//! use tole_desktop::app::{App, Boot, Limits};
//!
//! let app = App::start(&Boot::from_env()?)?; // returns at once
//! let view = app.view(); // `Starting` until the serve is settled
//! println!("{:?}", view.serve);
//! if app.take_change() {
//!     // read the view again and repaint
//! }
//! let quit = app.quit(Limits::default()); // closing the window
//! println!("{quit:?}");
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```
//!
//! # Threads
//!
//! | thread | does | blocks on |
//! |---|---|---|
//! | `tole-desktop-launch` (1, again after a failed launch) | `SupervisorLink::launch` | the serve's start-up, up to the supervisor's timeouts |
//! | `tole-desktop-command` (at most 1, short-lived) | the supervisor's `retry` or `restart_serve` | the supervisor's engine lock, the old serve stopping |
//! | the backend's (`backend` module docs) | the drivers | their calls |
//!
//! The window's thread blocks on none of them: [`App::view`] reads the backend's
//! snapshot, [`App::take_change`] is one atomic swap and the commands return at
//! once.
//!
//! # The wake-up
//!
//! The backend's `on_change` runs on its worker thread and the launch thread
//! ends on its own; both only raise one [`Signal`]. The window asks
//! [`App::take_change`] on its own timer and reads the view when it says `true`
//! (see [`Signal`]). A change reaches the window within the backend's
//! 100 ms pacing plus the window's look interval.
//!
//! # A launch that fails
//!
//! A supervisor that cannot start the serve reports it as a state
//! (`Failed(...)`), which the window shows with the button
//! [`ServeState::owner_action`](tole_desktop_model::ServeState::owner_action)
//! calls for. A launch that panics, or that cannot get a thread, is the same
//! kind of state (`Failed(StartFailed { detail })`), and "Try again" launches
//! once more. Neither is a panic of the app and neither leaves the window blank:
//! the window shows `Starting` from its first frame.
//!
//! # Not starting
//!
//! [`App::start`] refuses a folder that cannot be a workspace: `/` and the home
//! folder, which a file manager or a launcher starts an app in ([`Boot::refusal`]).
//! It returns a [`StartError`] with the reason, no serve is started, and the
//! window says so. It also refuses when the backend's threads cannot start, and
//! then the launch is never begun: the backend is started first.
//!
//! # Quitting
//!
//! [`App::quit`] never waits for the whole launch. In order: the backend is
//! shut down (it waits up to [`Limits::backend`] for the drivers, which are
//! stopped all at once), the link is closed (a launch still running is let go
//! and closes its serve on its own thread when it ends), and the serve gets
//! `close_window` on a thread the quit waits for up to [`Limits::serve`] (by
//! default the serve is left running, which is instant; the wait is for the
//! supervisor's monitor to stop). What did not finish in time is said in the
//! [`Quit`] report and ends with the process, and so is a serve that was only
//! released because something else still held it ([`ServeQuit::Held`]: a
//! setting that would have stopped it was not carried out).

mod boot;
mod launch;
mod quit;
mod signal;
mod window;

#[cfg(test)]
mod fake;
#[cfg(test)]
mod tests;
#[cfg(test)]
mod tests_quit;

use std::io;
use std::sync::Arc;

use tole_desktop_model::OwnerAction;

pub use self::boot::{Boot, StartError, Workspace};
pub use self::launch::Serve;
use self::launch::{supervisor_launcher, LaunchLink, Launcher};
pub use self::quit::{Limits, Quit, ServeQuit};
pub use self::signal::Signal;
use crate::backend::{Backend, BackendView, Config as BackendConfig};
use crate::driver::{Clock, SystemClock};

/// The running app: the backend, and the link to the serve it follows.
pub struct App {
    backend: Backend,
    link: Arc<LaunchLink>,
    signal: Signal,
}

impl App {
    /// Start the serve for `boot` (off this thread) and the backend, and
    /// return at once. `Err` when the folder the app was started in is not a
    /// workspace (`/` or the home folder) or when the system cannot start the
    /// backend's threads: in both cases nothing is running and no serve was
    /// started.
    pub fn start(boot: &Boot) -> Result<App, StartError> {
        if let Some(refused) = boot.refusal() {
            return Err(StartError::Workspace(refused));
        }
        let launcher = supervisor_launcher(boot.supervisor_config());
        App::start_with(launcher, Arc::new(SystemClock)).map_err(StartError::Threads)
    }

    pub(crate) fn start_with(launcher: Launcher, clock: Arc<dyn Clock>) -> io::Result<App> {
        let signal = Signal::new();
        let on_change = {
            let signal = signal.clone();
            Arc::new(move || signal.raise())
        };
        App::assemble(launcher, signal, |link| {
            Backend::start(
                link,
                clock,
                BackendConfig {
                    on_change: Some(on_change),
                    ..BackendConfig::default()
                },
            )
        })
    }

    /// The backend is started before the launch, so a backend that cannot
    /// start leaves no serve behind: the launch is never begun.
    fn assemble(
        launcher: Launcher,
        signal: Signal,
        start_backend: impl FnOnce(Arc<LaunchLink>) -> io::Result<Backend>,
    ) -> io::Result<App> {
        let link = LaunchLink::new(launcher, signal.clone());
        let backend = start_backend(link.clone())?;
        link.begin();
        Ok(App {
            backend,
            link,
            signal,
        })
    }

    /// The serve and what the drivers know, as of now. Cheap and never
    /// blocks. Counts the drivers do not know read `Unknown`, never zero.
    pub fn view(&self) -> BackendView {
        self.backend.snapshot()
    }

    /// Whether something changed since the last look; clears it. One atomic
    /// swap: call it as often as the window likes.
    pub fn take_change(&self) -> bool {
        self.signal.take()
    }

    /// The signal itself, for a window that wants to hand it to its own wake
    /// mechanism.
    pub fn signal(&self) -> Signal {
        self.signal.clone()
    }

    /// The button of a failure: [`OwnerAction::Retry`] or
    /// [`OwnerAction::RestartServe`]. Returns at once; the work is on a thread
    /// of its own and the result is the next view. One command runs at a time:
    /// a press while one runs is ignored, so the button looks dead until it
    /// ends.
    pub fn owner_action(&self, action: OwnerAction) {
        match action {
            OwnerAction::Retry => self.link.retry(),
            OwnerAction::RestartServe => self.link.restart_serve(),
        }
    }

    /// Whether the backend still runs (false after a panic ended it).
    pub fn is_running(&self) -> bool {
        self.backend.is_running()
    }

    /// The window is closing: see "Quitting" in the module docs.
    pub fn quit(self, limits: Limits) -> Quit {
        let App { backend, link, .. } = self;
        quit::quit(backend, &link, limits)
    }
}
