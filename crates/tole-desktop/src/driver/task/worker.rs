//! The worker thread: the only one that changes the state.

use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, SyncSender};
use std::sync::Arc;

use super::calls::Call;
use super::engine::{Effect, Engine, Input, Now, Out};
use super::store::Store;
use super::OnChange;
use crate::driver::clock::{Clock, Shutdown};

/// The worker thread's state: the engine and where its effects go.
pub(super) struct Worker {
    pub engine: Engine,
    pub inbox: Receiver<Input>,
    /// One channel per lane, in `LANES` order: a lane has at most one call
    /// out, so a send never waits.
    pub lanes: Vec<SyncSender<Call>>,
    /// Where the reader learns the cursor to open the stream from.
    pub open: SyncSender<u64>,
    pub clock: Arc<dyn Clock>,
    pub stop: Shutdown,
    pub store: Arc<Store>,
    pub alive: Arc<AtomicBool>,
    pub on_change: Option<OnChange>,
}

impl Worker {
    pub(super) fn run(mut self) {
        // A panic anywhere in the loop must not leave a state that claims to
        // be live: whatever ends the loop, the state is cleared after it, the
        // view reads stopped, and the other threads are told to stop.
        let _ = catch_unwind(AssertUnwindSafe(|| self.work()));
        self.stop.trigger();
        self.store.clear();
        self.alive.store(false, Ordering::SeqCst);
        self.changed();
    }

    fn now(&self) -> Now {
        Now {
            wall: self.clock.now_ms(),
            mono: self.clock.monotonic_ms(),
        }
    }

    fn work(&mut self) {
        let out = self.engine.start(self.now());
        if !self.carry_out(out) {
            return;
        }
        while let Ok(input) = self.inbox.recv() {
            if self.stop.is_set() {
                return;
            }
            let out = self.engine.step(input, self.now());
            if !self.carry_out(out) {
                return;
            }
        }
    }

    /// Hand the effects to the I/O threads and tell the UI. `false` when the
    /// driver must end: a thread is gone.
    fn carry_out(&self, out: Out) -> bool {
        let Out {
            changed,
            effects,
            fatal,
        } = out;
        if changed {
            self.changed();
        }
        !fatal
            && effects.into_iter().all(|effect| match effect {
                Effect::Call(lane, call) => {
                    (self.lanes.get(lane.index())).is_some_and(|tx| tx.send(call).is_ok())
                }
                Effect::OpenStream(after) => self.open.send(after).is_ok(),
            })
    }

    fn changed(&self) {
        if let Some(notify) = &self.on_change {
            // The UI's callback must not take the worker down with it.
            let _ = catch_unwind(AssertUnwindSafe(|| notify()));
        }
    }
}
