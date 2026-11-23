//! The worker thread: the only one that starts and stops drivers.

use std::collections::VecDeque;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::mpsc::Receiver;
use std::sync::Arc;
use std::time::Duration;

use super::engine::{Effect, Engine, Input, Observation};
use super::link::Link;
use super::live::{Live, Settings};
use super::shared::{Fanin, Msg, Shared};
use crate::driver::clock::{Clock, Shutdown};
use crate::driver::shared::handle::{self, OnChange};
use crate::driver::shared::now::{millis, Now};

/// How long a driver that is stopped is waited for.
pub(super) const DETACH_JOIN: Duration = Duration::from_millis(500);
/// The UI hears of a change at most this often (a change that comes sooner
/// waits for the next tick).
pub(super) const NOTIFY_GAP: Duration = Duration::from_millis(100);

/// The worker thread's state.
pub(super) struct Worker {
    pub engine: Engine,
    pub inbox: Receiver<Msg>,
    pub link: Link,
    pub clock: Arc<dyn Clock>,
    pub stop: Shutdown,
    pub shared: Arc<Shared>,
    pub fan: Arc<Fanin>,
    pub settings: Settings,
    pub on_change: Option<OnChange>,
    /// The drivers that run now.
    pub live: Option<Live>,
    /// When the UI was last told, on the monotonic clock.
    pub last_notified: Option<u64>,
}

impl Worker {
    pub(super) fn run(mut self) {
        // A panic anywhere in the loop must not leave a view that claims to be
        // known: whatever ends the loop, the drivers are stopped after it and
        // the view reads unknown.
        let _ = catch_unwind(AssertUnwindSafe(|| self.work()));
        self.stop.trigger();
        self.detach();
        self.shared.set_dead();
        handle::notify(&self.on_change);
    }

    fn work(&mut self) {
        loop {
            self.look_and_act();
            match self.inbox.recv() {
                Ok(Msg::ThreadFailed) | Err(_) => return,
                Ok(Msg::Tick | Msg::Wake) => {}
            }
            if self.stop.is_set() {
                return;
            }
        }
    }

    /// Read the link and the drivers, let the engine decide, carry out what it
    /// asks (and feed the results back to it), then tell the UI.
    fn look_and_act(&mut self) {
        let now = Now::read(&*self.clock);
        let look = self.link.look();
        let observation = Observation {
            state: look.state,
            epoch: look.epoch,
            drivers_alive: self.live.as_ref().is_none_or(Live::alive),
        };
        let mut queue = VecDeque::from([Input::Observe(observation)]);
        while let Some(input) = queue.pop_front() {
            let out = self.engine.step(input, now);
            if out.changed {
                self.fan.mark();
            }
            for effect in out.effects {
                match effect {
                    Effect::Attach { epoch } => {
                        queue.push_back(Input::Attached(self.attach(epoch)));
                    }
                    Effect::Detach => self.detach(),
                }
            }
        }
        self.notify(now.mono);
    }

    /// Start the drivers over a new `Api`. `false` when there is none yet or
    /// a driver cannot start.
    fn attach(&mut self, epoch: u64) -> bool {
        let Some(api) = self.link.api() else {
            return false;
        };
        let started = Live::start(api, &self.clock, epoch, self.settings, &self.fan);
        let Ok(live) = started else {
            return false;
        };
        self.shared.publish(Some(live.attached()));
        self.live = Some(live);
        true
    }

    /// Stop the drivers, if any. The view reads unknown from before the first
    /// of them is asked to stop, and no lock is held while they are.
    fn detach(&mut self) {
        self.shared.publish(None);
        if let Some(live) = self.live.take() {
            self.shared.add_report(live.shutdown(DETACH_JOIN));
        }
    }

    /// Tell the UI, if something changed and the last word was a while ago.
    fn notify(&mut self, mono: u64) {
        if !self.fan.is_pending() {
            return;
        }
        let soon = self
            .last_notified
            .is_some_and(|last| mono.saturating_sub(last) < millis(NOTIFY_GAP));
        if !soon && self.fan.take() {
            self.last_notified = Some(mono);
            handle::notify(&self.on_change);
        }
    }
}
