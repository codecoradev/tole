//! The worker thread: the only one that changes the state.

use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{sync_channel, Receiver, SyncSender};
use std::sync::Arc;

use super::calls::{Call, Lane};
use super::engine::{Effect, Engine, Input, Now, Out};
use super::store::Store;
use super::threads::spawn_lane;
use super::OnChange;
use crate::driver::api::Api;
use crate::driver::clock::{Clock, Shutdown};
use crate::driver::runtime::Spawner;

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
    /// What a replacement lane thread needs (see `Effect::ReplaceLane`).
    pub spawner: Spawner,
    pub api: Arc<dyn Api>,
    pub session: String,
    pub inbox_tx: SyncSender<Input>,
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
    fn carry_out(&mut self, out: Out) -> bool {
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
                Effect::ReplaceLane(lane) => self.replace(lane),
            })
    }

    /// The thread of `lane` is stuck in a call that was given up: its channel
    /// is closed (it ends when the call returns) and a new thread takes over.
    /// Once the driver is stopping nothing is started.
    pub(super) fn replace(&mut self, lane: Lane) -> bool {
        if self.stop.is_set() {
            return true;
        }
        let (tx, rx) = sync_channel::<Call>(1);
        let Some(slot) = self.lanes.get_mut(lane.index()) else {
            return false;
        };
        *slot = tx;
        let (api, session) = (Arc::clone(&self.api), self.session.clone());
        let ends = (self.inbox_tx.clone(), self.stop.clone());
        // A spawn refused because the driver began to stop is not a failure.
        spawn_lane(&self.spawner, api, session, lane, rx, ends).is_ok() || self.stop.is_set()
    }

    fn changed(&self) {
        if let Some(notify) = &self.on_change {
            // The UI's callback must not take the worker down with it.
            let _ = catch_unwind(AssertUnwindSafe(|| notify()));
        }
    }
}
