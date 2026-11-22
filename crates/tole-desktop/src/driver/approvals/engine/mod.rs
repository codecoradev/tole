//! The decisions of the approvals driver, with no thread and no I/O.
//!
//! [`Engine::step`] takes one [`Input`] (a stream item, a reply, a tick, an
//! owner press) and the two readings of time ([`Now`]), applies it to the model
//! through the adapter, and returns the [`Effect`]s the threads must carry out
//! (call `GET /approvals`, send a decision). The rules:
//!
//! - **Loss.** Every loss of knowledge is `ConnectionLost`, and a fetch of
//!   everything follows it (the deferred form of the adapter's `follow_up`:
//!   see `fetching`). Losses that arrive before that fetch starts are one.
//! - **Pace.** A fetch starts at most once per second, a failed one is retried
//!   after `Backoff` (1 s doubling to 30 s) and never abandoned, and a loss
//!   counts like a failure; the backoff starts over only when a fetch succeeds
//!   with every followed stream up. Deadlines are on the monotonic clock; a
//!   wall clock that jumps ahead of it (a suspend) counts as a loss.
//! - **Coverage.** The list is known only when a fetch that started after
//!   every followed session's stream was up (or is parked) has succeeded
//!   (`coverage`), and not
//!   when the last success is older than twice the refresh interval.
//! - **Sessions.** The followed set can change while the driver runs
//!   ([`Input::SetSessions`], see `sessions`): a new session is not covered
//!   until its stream is up and a fetch has followed, a removed one's reader is
//!   retired but keeps its stream slot until it ends, and never more than
//!   [`MAX_STREAMS`](super::MAX_STREAMS) readers are alive, retired ones
//!   included.
//! - **Answers.** A decision goes through `ApprovalsState::answer`, so it is
//!   sent once; before it leaves the queue it is checked against the model
//!   again (`deciding`), and what comes back is applied as the server's word or
//!   as a failure.
//! - **Time.** The model gets a `Tick` at least once a second from the worker's
//!   own clock reading, whatever else is queued (the model needs one every 5 s).
//!
//! Single flight: at most one `GET /approvals` and one decision call are out at
//! a time.

mod coverage;
mod deciding;
mod fetching;
mod sessions;

use std::collections::{BTreeMap, VecDeque};
use std::sync::Arc;
use std::time::Duration;

use tole_client::{ApprovalList, DecisionReply, Error, StreamItem};
use tole_desktop_model::approvals::{ApprovalsAction, Decision, DecisionIntent, FetchId, Tab};

use super::store::Store;
use crate::adapter::approvals::{stream_actions, FetchIds, TICK_MAX};
use crate::driver::shared::thread::DriverInput;

/// The model hears the time this often (at most [`TICK_MAX`]).
pub(super) const TICK_EVERY: Duration = Duration::from_secs(1);
const _: () = assert!(TICK_EVERY.as_millis() <= TICK_MAX.as_millis());

/// What reaches the engine.
pub(in crate::driver::approvals) enum Input {
    /// An item of a followed session's event stream.
    Stream(StreamItem),
    /// A followed session's stream delivered its first item: it is open.
    StreamUp(String),
    /// A followed session's stream ended or failed: knowledge is lost.
    StreamDown(String),
    /// A followed session's stream is refused again and again (404, `closed`,
    /// 429, ...): it no longer holds the list unknown, and is covered by the
    /// refresh only until its reader gets a stream open again (`StreamUp`).
    StreamParked(String),
    /// The clock moved on (the worker also keeps its own time).
    Tick,
    /// The reply of the `GET /approvals` the engine asked for with this id.
    Fetched(FetchId, Result<ApprovalList, Error>),
    /// The app wants these sessions followed, in place of the ones followed so
    /// far (already cut to size by `followed_sessions`, which the engine
    /// applies again).
    SetSessions(Vec<String>),
    /// A reader thread ended (its stream is closed, its slot is free).
    ReaderEnded(ReaderId),
    /// The owner pressed a button.
    Decide(String, Decision),
    /// The reply of the decision call the engine asked for.
    Decided(DecisionIntent, Result<DecisionReply, Error>),
    Select(Option<String>),
    SelectTab(Tab),
    /// Nothing to do: wakes the worker to look at its stop signal.
    Wake,
    /// A driver thread died by a panic outside any call: the driver ends.
    ThreadFailed,
}

/// What a thread must do for the engine.
#[derive(Debug, PartialEq, Eq)]
pub(in crate::driver::approvals) enum Effect {
    /// Call `GET /approvals` and answer with [`Input::Fetched`] and this id.
    Fetch(FetchId),
    /// Send this decision and answer with [`Input::Decided`].
    Send(DecisionIntent),
}

/// Names one reader thread for its whole life, so that a late message of a
/// reader that was replaced cannot be taken for the new one's.
pub(in crate::driver::approvals) type ReaderId = u64;

/// What the worker must do with the reader threads.
#[derive(Debug, PartialEq, Eq)]
pub(in crate::driver::approvals) enum ReaderOp {
    /// Start a reader for this session; it ends with [`Input::ReaderEnded`].
    Start(ReaderId, String),
    /// Tell this reader its session is no longer followed. It cannot be
    /// interrupted inside a read: it ends at its next frame, and its stream
    /// slot is held until then.
    Retire(ReaderId),
}

#[derive(Default)]
pub(in crate::driver::approvals) struct Out {
    /// Something a view shows changed.
    pub changed: bool,
    pub effects: Vec<Effect>,
    pub readers: Vec<ReaderOp>,
    /// The driver must end (a thread is gone).
    pub fatal: bool,
}

/// What the list owes to one followed session's stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Cover {
    Up,
    /// Not open: the list is not known until it is.
    Down,
    /// Refused again and again: covered by the refresh only (see `coverage`).
    Parked,
    /// Wanted, but with no reader yet: no stream slot is free, or the session's
    /// previous reader is still ending. Covered by the refresh only, like
    /// `Parked`, and ignores every stream message (none can be its own).
    Pending,
}

/// A reader thread that is alive.
struct Alive {
    session: String,
    /// Its session is no longer followed: it holds its slot until it ends.
    retired: bool,
}

/// A wall clock that moves further than the monotonic one by more than this
/// between two steps was suspended or corrected: what was known is lost.
pub(super) const JUMP_GAP_MS: u64 = 5_000;

pub(super) use crate::driver::shared::now::Now;

pub(in crate::driver::approvals) struct Engine {
    store: Arc<Store>,
    ids: FetchIds,
    backoff: crate::adapter::approvals::Backoff,
    refresh: Duration,
    last_tick: Option<u64>,
    // fetching
    /// The fetch the model trusts (the last one started, not lost since).
    current: Option<FetchId>,
    /// A `GET /approvals` is out.
    fetch_busy: bool,
    /// The coverage epoch when the current fetch started.
    fetch_epoch: u64,
    /// A fetch of everything must start, when the pace allows.
    want: bool,
    /// Earliest start of the next fetch (monotonic).
    not_before: u64,
    /// Failed fetches since the last success (for the view).
    failed_in_row: u32,
    /// When the last fetch that made the list known succeeded (monotonic).
    last_ok: Option<u64>,
    next_refresh: Option<u64>,
    /// A decision call failed in a way that may have landed: fetch soon. Cleared
    /// only when a fetch starts, so a fetch already out cannot swallow it.
    refresh_asap: bool,
    /// The time of the last step, to see the clock jump (suspend, correction).
    last_seen: Option<Now>,
    // coverage
    /// The followed sessions and what their stream gives the list.
    followed: BTreeMap<String, Cover>,
    /// Counts every change of which streams are up.
    epoch: u64,
    /// The reader threads that are alive, retired ones included: they hold the
    /// serve's stream slots, so the cap counts them (see `sessions`).
    readers: BTreeMap<ReaderId, Alive>,
    next_reader: ReaderId,
    // deciding
    decide_busy: bool,
    decide_queue: VecDeque<DecisionIntent>,
}

pub(super) use crate::driver::shared::now::millis;

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

impl Engine {
    pub(in crate::driver::approvals) fn new(
        store: Arc<Store>,
        sessions: Vec<String>,
        refresh: Duration,
    ) -> Self {
        Engine {
            store,
            ids: FetchIds::new(),
            backoff: crate::adapter::approvals::Backoff::new(),
            refresh,
            last_tick: None,
            current: None,
            fetch_busy: false,
            fetch_epoch: 0,
            want: false,
            not_before: 0,
            failed_in_row: 0,
            last_ok: None,
            next_refresh: None,
            refresh_asap: false,
            last_seen: None,
            followed: sessions.into_iter().map(|s| (s, Cover::Pending)).collect(),
            epoch: 0,
            readers: BTreeMap::new(),
            next_reader: 0,
            decide_busy: false,
            decide_queue: VecDeque::new(),
        }
    }

    /// The first step: nothing is known, the whole list is asked for at once.
    pub(in crate::driver::approvals) fn start(&mut self, now: Now) -> Out {
        self.want = true;
        self.not_before = now.mono;
        self.step(Input::Wake, now)
    }

    /// One input.
    pub(in crate::driver::approvals) fn step(&mut self, input: Input, now: Now) -> Out {
        let mut out = Out::default();
        if self.store.take_reset() {
            out.changed = true;
            self.lose(now, true);
        }
        out.changed |= self.check_jump(now);
        out.changed |= match input {
            Input::Stream(item) => stream_actions(&item, now.wall)
                .into_iter()
                .fold(false, |c, a| self.apply(a, now) | c),
            Input::StreamUp(session) => self.stream_up(&session),
            Input::StreamDown(session) => self.stream_down(&session, now),
            Input::StreamParked(session) => self.stream_parked(&session),
            Input::SetSessions(sessions) => self.set_sessions(sessions, &mut out),
            Input::ReaderEnded(id) => self.reader_ended(id),
            Input::Fetched(id, reply) => self.fetched(id, reply, now),
            Input::Decide(id, decision) => self.decide(&id, decision, now),
            Input::Decided(intent, reply) => self.decided(intent, reply, now),
            Input::Select(id) => self.apply(ApprovalsAction::Select(id), now),
            Input::SelectTab(tab) => self.apply(ApprovalsAction::SelectTab(tab), now),
            Input::ThreadFailed => {
                out.fatal = true;
                false
            }
            Input::Tick | Input::Wake => false,
        };
        out.changed |= self.housekeeping(now, &mut out);
        out
    }

    /// Apply an action; a loss goes through [`Engine::lose`].
    fn apply(&mut self, action: ApprovalsAction, now: Now) -> bool {
        match action {
            ApprovalsAction::ConnectionLost => self.lose(now, true),
            other => self.store.lock().apply(other),
        }
    }

    /// What is due after every input: the model's tick, the age of the list,
    /// the fetches the model or the schedule ask for, and the next decision.
    fn housekeeping(&mut self, now: Now, out: &mut Out) -> bool {
        let mut changed = false;
        let tick_due = self
            .last_tick
            .is_none_or(|t| now.mono >= t.saturating_add(millis(TICK_EVERY)));
        if tick_due {
            self.last_tick = Some(now.mono);
            changed |= self.store.lock().apply(ApprovalsAction::Tick(now.wall));
        }
        changed |= self.check_age(now);
        let asked = {
            let state = self.store.lock();
            state.needs_fetch() || state.is_stale()
        };
        let refresh_due = self.next_refresh.is_some_and(|t| now.mono >= t);
        if self.current.is_none() && (asked || refresh_due || self.refresh_asap) {
            self.want = true;
            self.next_refresh = None;
        }
        changed |= self.start_readers(now, out);
        changed |= self.maybe_fetch(now, out);
        changed |= self.dispatch_decisions(now, out);
        changed
    }
}

#[cfg(test)]
mod input_mapping {
    use super::{DriverInput, Input};

    /// A wrong mapping would compile and silently stop a thread panic from
    /// ending the driver, so the three are pinned.
    #[test]
    fn the_shared_thread_bodies_get_the_inputs_they_mean() {
        assert!(matches!(<Input as DriverInput>::tick(), Input::Tick));
        assert!(matches!(<Input as DriverInput>::wake(), Input::Wake));
        assert!(matches!(
            <Input as DriverInput>::thread_failed(),
            Input::ThreadFailed
        ));
    }
}
