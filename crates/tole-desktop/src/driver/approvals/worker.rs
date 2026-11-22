//! The worker thread: the only one that changes the state.

use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, SyncSender};
use std::sync::Arc;

use tole_desktop_model::approvals::{DecisionIntent, FetchId};

use super::engine::{Effect, Engine, Input, Now, Out};
use super::store::Store;
use super::OnChange;
use crate::driver::clock::{Clock, Shutdown};
use crate::driver::shared::handle;

/// The worker thread's state: the engine and where its effects go.
pub(super) struct Worker {
    pub engine: Engine,
    pub inbox: Receiver<Input>,
    pub fetch: SyncSender<FetchId>,
    pub send: SyncSender<DecisionIntent>,
    pub clock: Arc<dyn Clock>,
    pub stop: Shutdown,
    pub store: Arc<Store>,
    pub alive: Arc<AtomicBool>,
    pub on_change: Option<OnChange>,
}

impl Worker {
    pub(super) fn run(mut self) {
        // A panic anywhere in the loop must not leave a state that claims to
        // be known: whatever ends the loop, the state is cleared after it and
        // the other threads are told to stop.
        let _ = catch_unwind(AssertUnwindSafe(|| self.work()));
        self.stop.trigger();
        self.store.clear();
        self.alive.store(false, Ordering::SeqCst);
        self.changed();
    }

    fn now(&self) -> Now {
        Now::read(&*self.clock)
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
                Effect::Fetch(id) => self.fetch.send(id).is_ok(),
                Effect::Send(intent) => self.send.send(intent).is_ok(),
            })
    }

    fn changed(&self) {
        handle::notify(&self.on_change);
    }
}
