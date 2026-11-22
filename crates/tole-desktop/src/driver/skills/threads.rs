//! The bodies of the driver's threads. None decides anything: they block on a
//! call or a sleep and report what came back to the worker as an [`Input`].
//!
//! Each ends when the stop signal is raised, or when the worker is gone (its
//! inbox, or the channel it feeds this thread through, is closed).
//!
//! A panic never goes unnoticed. A panic inside an `Api` call is caught where
//! the call is made and becomes the failure of that call (`Loaded` with an
//! error), so the thread goes on and the engine retries. A panic anywhere else
//! in a thread is caught by [`run_guarded`], which tells the worker
//! (`ThreadFailed`); the worker then ends the driver, which leaves the view
//! not loaded.

use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::mpsc::{Receiver, SyncSender, TrySendError};
use std::sync::Arc;

use super::engine::{Input, Reply, Route, TICK_EVERY};
use crate::adapter::skills::LoadId;
use crate::driver::api::Api;
use crate::driver::clock::{Clock, Shutdown, Slept};
use crate::driver::guard::{guarded, panicked};

/// Run a thread body; if it panics, tell the worker so the driver ends.
pub(super) fn run_guarded(inbox: &SyncSender<Input>, body: impl FnOnce()) {
    if catch_unwind(AssertUnwindSafe(body)).is_err() {
        let _ = inbox.send(Input::ThreadFailed);
    }
}

pub(super) fn ticker(inbox: SyncSender<Input>, clock: Arc<dyn Clock>, stop: Shutdown) {
    while clock.sleep(TICK_EVERY, &stop) == Slept::Elapsed {
        // A full inbox drops the tick: whatever fills it wakes the worker, and
        // the worker looks at the time after every input.
        if let Err(TrySendError::Disconnected(_)) = inbox.try_send(Input::Tick) {
            return;
        }
    }
}

/// Runs the calls of one route the engine asks for, one at a time. The answer
/// is sent whatever it is (a full inbox waits: an answer is never dropped).
pub(super) fn loader(
    api: Arc<dyn Api>,
    route: Route,
    jobs: Receiver<LoadId>,
    inbox: SyncSender<Input>,
    stop: Shutdown,
) {
    for id in jobs {
        if stop.is_set() {
            return;
        }
        let reply = match route {
            Route::Skills => {
                Reply::Skills(guarded(|| api.skills()).unwrap_or_else(|| Err(panicked())))
            }
            Route::Tools => {
                Reply::Tools(guarded(|| api.tools()).unwrap_or_else(|| Err(panicked())))
            }
        };
        if inbox.send(Input::Loaded(id, reply)).is_err() {
            return;
        }
    }
}
