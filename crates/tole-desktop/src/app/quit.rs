//! Closing the window: every wait is bounded, and what did not finish says so.

use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::mpsc::channel;
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use super::launch::{CloseOutcome, LaunchLink, Serve};
use crate::backend::Backend;
use crate::driver::runtime::JoinReport;

/// How long each step of [`App::quit`](super::App::quit) may wait.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    /// For the backend's drivers. The backend stops them all at once, and a
    /// thread inside a call is let go after at most 500 ms.
    pub backend: Duration,
    /// For the serve's `close_window` (the supervisor's monitor stopping; a
    /// serve that is stopped on close would wait for it to exit).
    pub serve: Duration,
}

impl Default for Limits {
    fn default() -> Self {
        Limits {
            backend: Duration::from_millis(1500),
            serve: Duration::from_secs(2),
        }
    }
}

/// What happened to the serve when the window closed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ServeQuit {
    /// `close_window` returned in time and did what the settings say.
    Closed,
    /// Something still held the serve, so it was only released and left
    /// running: a setting that would have stopped it was not carried out.
    Held,
    /// `close_window` reported that the serve could not be stopped, panicked,
    /// or could not be run (no thread).
    Failed,
    /// `close_window` was still running when the wait ended; it ends with the
    /// process. The serve itself is not affected by the wait.
    Left,
    /// The launch was still running. It is let go and closes its own serve if
    /// it ever ends; the process ends before that, and a serve it had just
    /// started keeps running, to be attached to by the next launch.
    LaunchLetGo,
    /// There was no serve: the launch had failed.
    NoServe,
}

/// What [`App::quit`](super::App::quit) found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Quit {
    /// The backend's own threads and every driver it stopped.
    pub backend: JoinReport,
    pub serve: ServeQuit,
}

/// How [`bounded`] ended.
#[derive(Debug, PartialEq, Eq)]
enum Bounded<T> {
    Done(T),
    /// Still running when the wait ended; it is let go.
    Late,
    /// `f` panicked.
    Panicked,
    /// No thread could be started: `f` did not run.
    NotRun,
}

/// Run `f` on a thread of its own and wait up to `limit` for it.
fn bounded<T: Send + 'static>(
    name: &str,
    limit: Duration,
    f: impl FnOnce() -> T + Send + 'static,
) -> Bounded<T> {
    let (done, finished) = channel::<Option<T>>();
    let started = thread::Builder::new().name(name.into()).spawn(move || {
        let _ = done.send(catch_unwind(AssertUnwindSafe(f)).ok());
    });
    if started.is_err() {
        return Bounded::NotRun;
    }
    match finished.recv_timeout(limit) {
        Ok(Some(value)) => Bounded::Done(value),
        Ok(None) => Bounded::Panicked,
        // Timed out, or the thread ended without sending.
        Err(_) => Bounded::Late,
    }
}

pub(super) fn quit(backend: Backend, link: &Arc<LaunchLink>, limits: Limits) -> Quit {
    let report = backend.shutdown(limits.backend);
    let closing = link.close();
    let serve = match closing.serve {
        Some(serve) => close_serve(serve, limits.serve),
        None if closing.launching => ServeQuit::LaunchLetGo,
        None => ServeQuit::NoServe,
    };
    Quit {
        backend: report,
        serve,
    }
}

fn close_serve(serve: Arc<dyn Serve>, limit: Duration) -> ServeQuit {
    match bounded("tole-desktop-close", limit, move || serve.close_window()) {
        Bounded::Done(CloseOutcome::Closed) => ServeQuit::Closed,
        Bounded::Done(CloseOutcome::Held) => ServeQuit::Held,
        Bounded::Done(CloseOutcome::Failed) | Bounded::Panicked | Bounded::NotRun => {
            ServeQuit::Failed
        }
        Bounded::Late => ServeQuit::Left,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_job_that_finishes_panics_hangs_or_cannot_run_is_told_apart() {
        // Generous where the job must be seen to end (a loaded machine takes
        // a while to print a panic), short where it must be seen to hang.
        let patient = Duration::from_secs(20);
        assert_eq!(bounded("t-done", patient, || 7), Bounded::Done(7));
        assert_eq!(
            bounded("t-panic", patient, || -> u8 { panic!("boom") }),
            Bounded::Panicked
        );
        let (_hold, wait) = channel::<()>();
        assert_eq!(
            bounded("t-late", Duration::from_millis(100), move || {
                let _ = wait.recv();
            }),
            Bounded::Late
        );
    }
}
