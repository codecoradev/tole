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
//!   (a token): a status read from before an allow, or a queue read from
//!   before a reconnect, is dropped.
//! - **Never stuck busy.** While the model says a turn runs and nothing is
//!   waiting for its end (no prompt call out), a watcher polls `GET /status`
//!   (`watching`): after an allow `ResumePolling`, after anything else
//!   `BusyPolling::foreign`, after a Stop `BusyPolling::after_stop`. A watcher
//!   that ends while the model is still busy is replaced, slowly.
//! - **Sending.** A prompt, a steer note, a Stop and an answer each have one
//!   call in flight and go through the model's own rules (`sending`,
//!   `deciding`); a failed call gives the owner's words back and asks the
//!   serve whether the turn runs anyway.
//! - **Time.** Every deadline is on the monotonic clock. The wall clock is
//!   only read to see a suspend (it ran ahead of the monotonic one): the
//!   status and the queue are read again then.

mod abandon;
mod deciding;
mod history;
mod ids;
mod refresh;
mod sending;
mod stream;
mod watching;

use std::cell::RefCell;
use std::sync::Arc;
use std::time::Duration;

use tole_client::{Error, StreamItem};
use tole_desktop_model::approvals::Decision;
use tole_desktop_model::task::{Link, StatusRow, TaskEvent};

use super::calls::{Call, Lane, Reply};
use super::store::Store;
use super::Config;
use crate::adapter::task::EntryMapper;
use crate::driver::shared::thread::DriverInput;

/// The worker wakes the engine this often when nothing else does (the polls
/// of a watcher are 500 ms apart).
pub(in crate::driver::task) const TICK_EVERY: Duration = Duration::from_millis(250);

/// A wall clock that moves further than the monotonic one by more than this
/// between two steps was suspended or corrected.
const JUMP_GAP_MS: u64 = 5_000;

/// What reaches the engine.
pub(in crate::driver::task) enum Input {
    Send(String),
    Steer(String),
    Stop,
    Decide(String, Decision),
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
    /// The thread of this lane is stuck in a call that was given up: start
    /// another for the next call (the old one ends when its call returns).
    ReplaceLane(Lane),
}

#[derive(Default)]
pub(in crate::driver::task) struct Out {
    /// Something a view shows changed.
    pub changed: bool,
    pub effects: Vec<Effect>,
    /// The driver must end (a thread is gone, or the state is broken).
    pub fatal: bool,
}

pub(in crate::driver::task) use crate::driver::shared::now::Now;

/// What a view shows of the model that a poll could change.
#[derive(PartialEq)]
struct Glance {
    status: StatusRow,
    rows: usize,
    link: Link,
    cards: Vec<String>,
    stale: bool,
    undecidable: Vec<String>,
}

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

pub(in crate::driver::task) struct Engine {
    store: Arc<Store>,
    session: String,
    fresh: bool,
    mapper: EntryMapper,
    last_seen: Option<Now>,
    /// A call is out on this lane (indexed by `Lane::index`).
    out_on: [bool; 6],
    hist: history::History,
    sending: sending::Sending,
    watch: watching::Watching,
    refresh: refresh::Refresh,
    deciding: deciding::Deciding,
    /// The serve's own ids of the cards the model shows.
    ids: RefCell<ids::ApprovalIds>,
    link: stream::Link,
}

impl Engine {
    pub(in crate::driver::task) fn new(store: Arc<Store>, cfg: &Config) -> Self {
        Engine {
            store,
            session: cfg.session.clone(),
            fresh: cfg.fresh,
            mapper: Default::default(),
            last_seen: None,
            out_on: [false; 6],
            hist: Default::default(),
            sending: Default::default(),
            watch: Default::default(),
            refresh: Default::default(),
            deciding: Default::default(),
            ids: Default::default(),
            link: Default::default(),
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
            Input::Send(text) => self.accepted(|e| e.send(text, &mut out)),
            Input::Steer(text) => self.accepted(|e| e.steer(text, &mut out)),
            Input::Stop => self.accepted(|e| e.stop(&mut out)),
            Input::Decide(id, decision) => self.accepted(|e| e.decide(&id, decision)),
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

    /// Run a command; one the engine refuses is counted for the view.
    fn accepted(&mut self, command: impl FnOnce(&mut Self) -> bool) -> bool {
        if command(self) {
            return true;
        }
        self.store.lock().facts.ignored += 1;
        true
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
            undecidable: shared.facts.undecidable.clone(),
        }
    }

    /// Apply a reply; whether the view changed. Every reply but a poll of the
    /// status or the queue counts as a change: a lane was freed, and the
    /// view's `deciding`, send mode or `loading` may have moved with it.
    fn reply(&mut self, reply: Reply, now: Now, out: &mut Out) -> bool {
        let quiet = matches!(reply, Reply::Status { .. } | Reply::Approvals(..));
        let before = quiet.then(|| self.glance());
        let _ = match reply {
            Reply::Entries(after, result) => self.history_reply(after, result, now, out),
            Reply::Approvals(token, result) => self.refresh_reply(token, result, now),
            Reply::Status {
                token,
                check,
                reply,
            } => self.status_reply(token, check, reply, now),
            Reply::Prompt(generation, result) => self.prompt_reply(generation, result, now),
            Reply::Cancel(result) => self.cancel_reply(result, now),
            Reply::Interrupt(result) => self.steer_reply(result, now),
            Reply::Decide(id, result) => self.decide_reply(&id, result, now),
        };
        before.is_none_or(|before| before != self.glance())
    }

    /// What is due after every input: the next history page, the watcher,
    /// the status and queue reads, the next answer.
    fn housekeeping(&mut self, now: Now, out: &mut Out) -> bool {
        if self.store.lock().task.run.closed {
            // The session left the serve: nothing it could answer is asked.
            self.refresh.cancel();
            self.watch.cancel_check();
        } else {
            self.poll_while_not_live(now);
        }
        let mut changed = self.history_next(now, out);
        changed |= self.prompt_abandon(now, out);
        changed |= self.watch_sync(now);
        changed |= self.watch_tick(now);
        self.status_next(now, out);
        self.refresh_next(now, out);
        changed |= self.decide_next(out);
        changed
    }

    /// The wall clock ran ahead of the monotonic one: the machine slept (the
    /// monotonic clock stops in a suspend) or the clock was corrected. A remote
    /// stream may be dead and the turn over: read the status and the queue.
    fn check_jump(&mut self, now: Now) {
        let prev = self.last_seen.replace(now);
        let Some(prev) = prev else { return };
        let wall = now.wall.saturating_sub(prev.wall);
        let mono = now.mono.saturating_sub(prev.mono);
        if wall > mono.saturating_add(JUMP_GAP_MS) {
            self.sending.jumped = true;
            self.refresh.want(now);
            self.watch.check_soon(now);
        }
    }

    /// Fold events into the model, under one lock. Returns whether there were any.
    fn apply(&self, events: Vec<TaskEvent>) -> bool {
        let any = !events.is_empty();
        // Text deltas and reasoning marks never touch the queue of cards: the
        // hot path of a long answer skips the pass over the cards.
        let cards_untouched = events
            .iter()
            .all(|e| matches!(e, TaskEvent::TextDelta { .. } | TaskEvent::Reasoning));
        self.ids.borrow_mut().note(&events);
        let mut shared = self.store.lock();
        events.into_iter().for_each(|e| shared.task.apply(e));
        if cards_untouched {
            return any;
        }
        let live: Vec<String> = shared
            .task
            .run
            .approvals
            .iter()
            .map(|a| a.id.clone())
            .collect();
        let mut ids = self.ids.borrow_mut();
        ids.prune(&live);
        shared.facts.undecidable = ids.unanswerable(&live);
        any
    }

    /// Mark `lane` busy; `false` when a call is already out on it.
    fn take_lane(&mut self, lane: Lane) -> bool {
        !std::mem::replace(&mut self.out_on[lane.index()], true)
    }

    fn free_lane(&mut self, lane: Lane) {
        self.out_on[lane.index()] = false;
    }

    fn lane_busy(&self, lane: Lane) -> bool {
        self.out_on[lane.index()]
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
