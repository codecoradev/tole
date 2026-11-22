//! The decisions of the skills driver, with no thread and no I/O.
//!
//! [`Engine::step`] takes one [`Input`] (an answer, a tick, an owner press) and
//! the two readings of time ([`Now`]), applies it to the model through the
//! adapter, and returns the [`Effect`]s the threads must carry out (call
//! `GET /skills`, call `GET /tools`). The rules:
//!
//! - **Two tabs, two schedules.** The Skills tab and the Connected tools tab
//!   load from their own routes with their own pace and backoff, so a tab that
//!   fails does not hold back the other. At most one call of each is out.
//! - **First load.** At once, from `start`.
//! - **Refresh.** A slow timer ([`Config::refresh`](super::Config::refresh))
//!   from the last answer, and `Input::Refresh` (the owner asked): the call
//!   starts as soon as the pace allows, a call that is already out does not
//!   count (it may have read before the change the owner is waiting for), and
//!   any number of presses are one more call.
//! - **Pace.** At most one call of a route a second. A failed call is retried
//!   after `Backoff` (1 s doubling to 30 s), never abandoned; the backoff
//!   starts over when a call succeeds. An owner's refresh skips the backoff
//!   wait, not the pace.
//! - **Failure.** The answer of the call goes through `LoadIds` and the
//!   adapter: an error empties its tab and says why, and a `notes` line makes
//!   it partial (badge `Unknown`). A failed or partial read is never a zero.
//! - **Age.** Rows that were read more than twice the refresh interval ago
//!   (calls that hang, a timer that stalled) are taken back like a failure.
//! - **Sleep.** The wall clock moving ahead of the monotonic one by more than a
//!   few seconds (the machine slept, the clock was corrected) is a loss: the
//!   rows may be hours old, so they are taken back, the call that is out is
//!   dropped (its answer is ignored) and a new one is wanted.
//!
//! Deadlines are on the monotonic clock; the wall clock is read only to see the
//! jump.

mod lane;
mod loading;

use std::sync::Arc;
use std::time::Duration;

use tole_desktop_model::Tab;

use self::lane::Lane;
pub(super) use self::loading::Reply;
use super::store::Store;
use crate::adapter::skills::{LoadId, LoadIds};
use crate::driver::shared::thread::DriverInput;

/// The worker wakes at least this often (a refresh, a backoff or the age of the
/// rows can fall due while the inbox is silent).
pub(super) const TICK_EVERY: Duration = Duration::from_secs(1);

/// A wall clock that moves further than the monotonic one by more than this
/// between two steps was suspended or corrected: what was loaded is lost.
const JUMP_GAP_MS: u64 = 5_000;

/// What the page says when it takes rows back for being too old.
pub(super) const STALE: &str = "Not refreshed in time: the list may be out of date";

/// The two routes of the driver.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Route {
    Skills,
    Tools,
}

impl Route {
    const ALL: [Route; 2] = [Route::Skills, Route::Tools];

    fn index(self) -> usize {
        self as usize
    }
}

/// What reaches the engine.
pub(super) enum Input {
    /// The answer of the call the engine asked for with this id.
    Loaded(LoadId, Reply),
    /// The owner asked to load both tabs now.
    Refresh,
    SelectTab(Tab),
    /// The clock moved on (the worker also keeps its own time).
    Tick,
    /// Nothing to do: wakes the worker to look at its stop signal.
    Wake,
    /// A driver thread died by a panic outside any call: the driver ends.
    ThreadFailed,
}

/// What a thread must do for the engine.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum Effect {
    /// Call the route and answer with [`Input::Loaded`] and this id.
    Load(Route, LoadId),
}

#[derive(Default)]
pub(super) struct Out {
    /// Something a view shows changed.
    pub changed: bool,
    pub effects: Vec<Effect>,
    /// The driver must end (a thread is gone).
    pub fatal: bool,
}

pub(super) use crate::driver::shared::now::Now;

pub(super) use crate::driver::clock::millis;

impl DriverInput for Input {
    fn tick() -> Self {
        Input::Tick
    }
    fn wake() -> Self {
        Input::Wake
    }
    fn thread_failed() -> Self {
        Input::ThreadFailed
    }
}

pub(super) struct Engine {
    store: Arc<Store>,
    ids: LoadIds,
    refresh: Duration,
    /// One schedule per route, by `Route::index`.
    lanes: [Lane; 2],
    /// The time of the last step, to see the clock jump.
    last_seen: Option<Now>,
}

impl Engine {
    pub(super) fn new(store: Arc<Store>, refresh: Duration) -> Self {
        Engine {
            store,
            ids: LoadIds::new(),
            refresh,
            lanes: [Lane::new(), Lane::new()],
            last_seen: None,
        }
    }

    /// The first step: nothing is loaded, both tabs are asked for at once.
    pub(super) fn start(&mut self, now: Now) -> Out {
        self.step(Input::Wake, now)
    }

    /// One input.
    pub(super) fn step(&mut self, input: Input, now: Now) -> Out {
        let mut out = Out::default();
        if self.store.take_reset() {
            // The state was thrown away: nothing is loaded, and what is out
            // describes a page that is gone.
            out.changed = true;
            self.lose_all(true);
        }
        out.changed |= self.check_jump(now);
        out.changed |= match input {
            Input::Loaded(id, reply) => self.loaded(id, reply, now),
            Input::Refresh => {
                self.lanes.iter_mut().for_each(|l| l.asap = true);
                false
            }
            Input::SelectTab(tab) => {
                let mut shared = self.store.lock();
                let changed = shared.page.tab != tab;
                shared.page.select_tab(tab);
                changed
            }
            Input::ThreadFailed => {
                out.fatal = true;
                false
            }
            Input::Tick | Input::Wake => false,
        };
        out.changed |= self.housekeeping(now, &mut out);
        out
    }

    fn lose_all(&mut self, drop_call: bool) -> bool {
        Route::ALL
            .into_iter()
            .fold(false, |c, r| self.lose(r, drop_call) | c)
    }

    /// The wall clock moved further than the monotonic one: the machine slept
    /// (the monotonic clock stops in a suspend) or the clock was corrected
    /// forward. Not scheduling: only this detection reads the wall delta.
    fn check_jump(&mut self, now: Now) -> bool {
        let Some(prev) = self.last_seen.replace(now) else {
            return false;
        };
        let wall = now.wall.saturating_sub(prev.wall);
        let mono = now.mono.saturating_sub(prev.mono);
        wall > mono.saturating_add(JUMP_GAP_MS) && self.lose_all(true)
    }

    /// What is due after every input: rows that are too old, the refreshes and
    /// retries that fell due, and the calls that may start.
    fn housekeeping(&mut self, now: Now, out: &mut Out) -> bool {
        let limit = millis(self.refresh).saturating_mul(2);
        let mut changed = false;
        for route in Route::ALL {
            if self.lanes[route.index()].aged(now.mono, limit) {
                changed |= self.lose(route, false);
            }
            self.lanes[route.index()].schedule(now.mono);
            changed |= self.begin(route, now, out);
        }
        changed
    }
}
