//! The thread bodies and the stop sequence every driver runs. A driver names
//! its `Input` through [`DriverInput`] and keeps the rest (its tick period,
//! its other threads) to itself.

use std::io;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::mpsc::{SyncSender, TrySendError};
use std::sync::Arc;
use std::time::Duration;

use crate::driver::clock::{Clock, Shutdown, Slept};
use crate::driver::runtime::{JoinReport, Threads};

/// How long dropping a driver waits for its threads.
pub(crate) const DROP_JOIN: Duration = Duration::from_millis(500);

/// The three messages every driver's worker takes from its threads.
pub(crate) trait DriverInput: Send + 'static {
    /// The clock moved on.
    fn tick() -> Self;
    /// Nothing to do: wakes the worker to look at its stop signal.
    fn wake() -> Self;
    /// A driver thread died by a panic outside any call: the driver ends.
    fn thread_failed() -> Self;
}

/// Run a thread body; if it panics, tell the worker so the driver ends.
pub(crate) fn run_guarded<M: DriverInput>(inbox: &SyncSender<M>, body: impl FnOnce()) {
    if catch_unwind(AssertUnwindSafe(body)).is_err() {
        let _ = inbox.send(M::thread_failed());
    }
}

/// Sends a tick every `every` on the clock's monotonic reading, until the stop
/// signal or until the worker is gone.
fn ticker<M: DriverInput>(
    inbox: SyncSender<M>,
    clock: Arc<dyn Clock>,
    stop: Shutdown,
    every: Duration,
) {
    while clock.sleep(every, &stop) == Slept::Elapsed {
        // A full inbox drops the tick: the worker keeps its own time and reads
        // the clock at every input, so a tick only wakes it when idle.
        if let Err(TrySendError::Disconnected(_)) = inbox.try_send(M::tick()) {
            return;
        }
    }
}

/// Start the named tick thread of a driver, guarded by [`run_guarded`].
pub(crate) fn spawn_ticker<M: DriverInput>(
    threads: &mut Threads,
    name: &str,
    inbox: SyncSender<M>,
    clock: Arc<dyn Clock>,
    stop: Shutdown,
    every: Duration,
) -> io::Result<()> {
    threads.spawn(name, move || {
        run_guarded(&inbox.clone(), || ticker(inbox, clock, stop, every))
    })
}

/// Stop every thread of a driver and wait up to `timeout` for them: raise the
/// stop signal, wake the worker if it is waiting on its inbox, then join.
pub(crate) fn stop_threads<M: DriverInput>(
    stop: &Shutdown,
    inbox: &SyncSender<M>,
    threads: &mut Threads,
    timeout: Duration,
) -> JoinReport {
    stop.trigger();
    let _ = inbox.try_send(M::wake());
    threads.join(timeout)
}
