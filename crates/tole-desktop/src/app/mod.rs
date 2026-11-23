//! The app: what boots the desktop and what the window reads and calls.
//!
//! No toolkit type is here and no window: [`App`] starts the local serve off
//! the window's thread, keeps the [`Backend`] following it, and gives the
//! window three things: [`App::view`] (plain data), [`App::take_change`] (a
//! change since the last look) and [`App::quit`].
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
//! | `tole-desktop-launch` (1) | `SupervisorLink::launch` | the serve's start-up, up to the supervisor's timeouts |
//! | the backend's (`backend` module docs) | the drivers | their calls |
//!
//! The window's thread blocks on none of them: [`App::view`] reads the backend's
//! snapshot and [`App::take_change`] is one atomic swap.
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
//! (`Failed(...)`). A launch that panics, or that cannot get a thread, is the
//! same kind of state (`Failed(StartFailed { detail })`). Neither is a panic of
//! the app and neither leaves the window blank: the window shows `Starting` from
//! its first frame.
//!
//! # Not starting
//!
//! The backend is started before the launch begins, so a backend that cannot
//! start (no threads) leaves no serve behind: [`App::start`] returns the error
//! and nothing is running.
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

#[cfg(test)]
mod fake;
#[cfg(test)]
mod tests;
#[cfg(test)]
mod tests_quit;

use std::io;
use std::sync::Arc;

pub use self::boot::Boot;
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
    /// return at once. `Err` only when the system cannot start the backend's
    /// threads: nothing is running and no serve was started then.
    pub fn start(boot: &Boot) -> io::Result<App> {
        let launcher = supervisor_launcher(boot.supervisor_config());
        App::start_with(launcher, Arc::new(SystemClock))
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
