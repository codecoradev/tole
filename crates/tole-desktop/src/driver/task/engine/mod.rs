//! The decisions of the task driver, with no thread and no I/O.
//!
//! [`Engine::step`] takes one [`Input`] (an owner's command, a stream item, a
//! call's reply, a tick) and the two readings of time ([`Now`]), applies it to
//! the model through the adapter, and returns the [`Effect`]s the threads must
//! carry out (make a call, open the stream). The rules:
//!
//! - **Order.** The history is read first, page by page (`history`); only
//!   then is the stream opened, from the cursor the history ended on, so a live
//!   entry can never reach the model before an older one. The queue of
//!   approvals is read once the stream is up (`refresh`).
//! - **Single flight.** At most one call is out on each lane (`calls::Lane`),
//!   and a reply is applied only when it still answers what the engine asked
//!   (a token): a queue read from before a reconnect is dropped.
//! - **Time.** Every deadline is on the monotonic clock. The wall clock is
//!   only read to see a suspend (it ran ahead of the monotonic one): the
//!   queue is read again then.

mod history;
mod refresh;
mod stream;

use std::sync::Arc;
use std::time::Duration;

use tole_client::{Error, StreamItem};
use tole_desktop_model::task::{Link, StatusRow, TaskEvent};

use super::calls::{Call, Lane, Reply, LANES};
use super::store::Store;
use super::Config;
use crate::adapter::task::EntryMapper;

/// The worker wakes the engine this often when nothing else does (the polls
/// of a watcher are 500 ms apart).
pub(in crate::driver::task) const TICK_EVERY: Duration = Duration::from_millis(250);

/// A wall clock that moves further than the monotonic one by more than this
/// between two steps was suspended or corrected.
const JUMP_GAP_MS: u64 = 5_000;

/// What reaches the engine.
pub(in crate::driver::task) enum Input {
    Heights(Vec<(usize, f32)>),
    Toggle(usize),
    /// A stream delivered its first item; `reopened` after a loss.
    Up {
        reopened: bool,
    },
    Stream(Result<Option<StreamItem>, Error>),
    /// The serve keeps refusing the stream: it is probed slowly now.
    Parked,
    Reply(Reply),
    Tick,
    /// Nothing to do: wakes the worker to look at its stop signal.
    Wake,
    /// A driver thread died by a panic outside any call: the driver ends.
    ThreadFailed,
}

/// What a thread must do for the engine.
pub(in crate::driver::task) enum Effect {
    Call(Lane, Call),
    /// Open the event stream after this entry.
    OpenStream(u64),
}

#[derive(Default)]
pub(in crate::driver::task) struct Out {
    /// Something a view shows changed.
    pub changed: bool,
    pub effects: Vec<Effect>,
    /// The driver must end (a thread is gone, or the state is broken).
    pub fatal: bool,
}

/// The two readings of time at one moment. Deadlines use `mono`; the wall
/// clock only feeds the suspend check.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::driver::task) struct Now {
    pub wall: u64,
    pub mono: u64,
}

/// What a view shows of the model that a poll could change.
#[derive(PartialEq)]
struct Glance {
    status: StatusRow,
    rows: usize,
    link: Link,
    cards: Vec<String>,
    stale: bool,
}

pub(super) fn millis(d: Duration) -> u64 {
    u64::try_from(d.as_millis()).unwrap_or(u64::MAX)
}

pub(in crate::driver::task) struct Engine {
    store: Arc<Store>,
    session: String,
    fresh: bool,
    mapper: EntryMapper,
    last_seen: Option<Now>,
    /// A call is out on this lane (indexed by `Lane::index`).
    out_on: [bool; LANES.len()],
    hist: history::History,
    refresh: refresh::Refresh,
}

impl Engine {
    pub(in crate::driver::task) fn new(store: Arc<Store>, cfg: &Config) -> Self {
        Engine {
            store,
            session: cfg.session.clone(),
            fresh: cfg.fresh,
            mapper: Default::default(),
            last_seen: None,
            out_on: [false; LANES.len()],
            hist: Default::default(),
            refresh: Default::default(),
        }
    }

    /// The first step: read the history, or, for a session that has none,
    /// open the stream at once.
    pub(in crate::driver::task) fn start(&mut self, now: Now) -> Out {
        let mut out = Out {
            changed: true,
            ..Out::default()
        };
        self.last_seen = Some(now);
        if self.fresh {
            self.finish_history(true, &mut out);
        } else {
            self.hist.begin();
            self.store.lock().facts.loading = true;
        }
        out.changed |= self.housekeeping(now, &mut out);
        out
    }

    /// One input.
    pub(in crate::driver::task) fn step(&mut self, input: Input, now: Now) -> Out {
        let mut out = Out::default();
        if self.store.take_broken() {
            out.fatal = true;
            return out;
        }
        self.check_jump(now);
        out.changed = match input {
            Input::Heights(heights) => self.heights(&heights),
            Input::Toggle(index) => {
                self.store.lock().task.toggle_group(index);
                true
            }
            Input::Up { reopened } => self.up(reopened, now),
            Input::Stream(item) => self.stream(item, now),
            Input::Parked => {
                self.store.lock().facts.parked = true;
                true
            }
            Input::Reply(reply) => self.reply(reply, now, &mut out),
            Input::ThreadFailed => {
                out.fatal = true;
                false
            }
            Input::Tick | Input::Wake => false,
        };
        out.changed |= self.housekeeping(now, &mut out);
        out
    }

    /// What the view shows of the model, to tell a reply that changed it from
    /// one that did not (a status poll that says what was already known).
    fn glance(&self) -> Glance {
        let shared = self.store.lock();
        let (task, run) = (&shared.task, &shared.task.run);
        Glance {
            status: task.status(),
            rows: task.transcript.items.len(),
            link: run.link,
            cards: run.approvals.iter().map(|a| a.id.clone()).collect(),
            stale: run.approvals_stale,
        }
    }

    /// Apply a reply; whether the view changed. Every reply but a poll of the
    /// queue counts as a change: a lane was freed, and the view's `loading`
    /// may have moved with it.
    fn reply(&mut self, reply: Reply, now: Now, out: &mut Out) -> bool {
        let quiet = matches!(reply, Reply::Approvals(..));
        let before = quiet.then(|| self.glance());
        let _ = match reply {
            Reply::Entries(after, result) => self.history_reply(after, result, now, out),
            Reply::Approvals(token, result) => self.refresh_reply(token, result, now),
        };
        before.is_none_or(|before| before != self.glance())
    }

    /// What is due after every input: the next history page, the queue read.
    fn housekeeping(&mut self, now: Now, out: &mut Out) -> bool {
        if self.store.lock().task.run.closed {
            // The session left the serve: nothing it could answer is asked.
            self.refresh.cancel();
        }
        let changed = self.history_next(now, out);
        self.refresh_next(now, out);
        changed
    }

    /// The wall clock ran ahead of the monotonic one: the machine slept (the
    /// monotonic clock stops in a suspend) or the clock was corrected. A remote
    /// stream may be dead: read the queue.
    fn check_jump(&mut self, now: Now) {
        let prev = self.last_seen.replace(now);
        let Some(prev) = prev else { return };
        let wall = now.wall.saturating_sub(prev.wall);
        let mono = now.mono.saturating_sub(prev.mono);
        if wall > mono.saturating_add(JUMP_GAP_MS) {
            self.refresh.want(now);
        }
    }

    /// Fold events into the model, under one lock. Returns whether there were any.
    fn apply(&self, events: Vec<TaskEvent>) -> bool {
        let any = !events.is_empty();
        let mut shared = self.store.lock();
        events.into_iter().for_each(|e| shared.task.apply(e));
        any
    }

    /// Mark `lane` busy; `false` when a call is already out on it.
    fn take_lane(&mut self, lane: Lane) -> bool {
        !std::mem::replace(&mut self.out_on[lane.index()], true)
    }

    fn free_lane(&mut self, lane: Lane) {
        self.out_on[lane.index()] = false;
    }

    fn heights(&mut self, heights: &[(usize, f32)]) -> bool {
        let mut shared = self.store.lock();
        // A bounded batch: the UI measures the rows it built, a few dozen.
        // Only a height that moved is a change: a repaint that measures the
        // same rows again must not ask for another repaint.
        let mut moved = false;
        for (row, height) in heights.iter().take(1_000) {
            moved |= shared.task.layout.set_height(*row, *height);
        }
        moved
    }
}
