//! The schedule of one route's loads: when the next call may start and what
//! each outcome does to it. Pure bookkeeping on the monotonic clock; the page
//! and the replies are the engine's. Shared by the drivers that load a list on
//! a timer (skills, automations); `I` is the driver's id of a call.

use std::time::Duration;

use super::now::millis;
use crate::adapter::approvals::{Backoff, RETRY_MIN};

pub(crate) struct Lane<I: Copy + PartialEq> {
    /// The call the page trusts: the one started last, not lost since.
    pub current: Option<I>,
    /// The call whose answer is awaited (also after it was lost: its thread is
    /// still in it). The one slot of the route: only its answer frees it.
    pub out: Option<I>,
    /// A load must start, when the pace allows.
    pub want: bool,
    /// The owner asked for a load now: it ignores the backoff wait (not the
    /// one-a-second pace), and a call already out does not satisfy it. Cleared
    /// only when a call starts.
    pub asap: bool,
    /// Earliest start of the next call (monotonic): one a second, whatever asks.
    floor: u64,
    /// Earliest start after a failure: the backoff.
    not_before: u64,
    backoff: Backoff,
    /// When the data on the page was read (monotonic), if it is there.
    pub last_ok: Option<u64>,
    next_refresh: Option<u64>,
}

impl<I: Copy + PartialEq> Lane<I> {
    /// Nothing is loaded: the first call is wanted at once.
    pub(crate) fn new() -> Self {
        Lane {
            current: None,
            out: None,
            want: true,
            asap: false,
            floor: 0,
            not_before: 0,
            backoff: Backoff::new(),
            last_ok: None,
            next_refresh: None,
        }
    }

    pub(crate) fn may_start(&self, mono: u64) -> bool {
        self.want
            && self.out.is_none()
            && mono >= self.floor
            && (mono >= self.not_before || self.asap)
    }

    pub(crate) fn started(&mut self, id: I, mono: u64) {
        self.want = false;
        self.asap = false;
        self.out = Some(id);
        self.current = Some(id);
        self.floor = mono.saturating_add(millis(RETRY_MIN));
    }

    /// A refresh is due, or the owner asked for one, and nothing trusted is
    /// out: a load is wanted.
    pub(crate) fn schedule(&mut self, mono: u64) {
        let due = self.next_refresh.is_some_and(|t| mono >= t);
        if self.current.is_none() && (self.asap || due) {
            self.want = true;
            self.next_refresh = None;
        }
    }

    /// The call of `current` answered with data that is on the page now.
    pub(crate) fn succeeded(&mut self, mono: u64, refresh: Duration) {
        self.current = None;
        self.want = false;
        self.backoff.succeeded();
        self.not_before = 0;
        self.last_ok = Some(mono);
        self.next_refresh = Some(mono.saturating_add(millis(refresh)));
    }

    /// The call of `current` failed (or its answer could not be used): retry
    /// after the next backoff step, for ever.
    pub(crate) fn failed(&mut self, mono: u64) {
        self.current = None;
        self.want = true;
        self.not_before = mono.saturating_add(millis(self.backoff.failed()));
        self.last_ok = None;
        self.next_refresh = None;
    }

    /// What was loaded is not to be trusted any more. With `drop_call` the call
    /// that is out is forgotten too (its answer is ignored when it comes), and
    /// a new one is wanted; without it (the data is old only because that call
    /// is slow) its answer is awaited.
    pub(crate) fn lost(&mut self, drop_call: bool) {
        if drop_call {
            self.current = None;
        }
        self.last_ok = None;
        self.next_refresh = None;
        if self.current.is_none() {
            self.want = true;
        }
    }

    /// Something changed on the serve that the call which is out may have read
    /// before (a switch was answered): its answer is no longer trusted, and a
    /// new call is wanted at once (not after a backoff). What is on the page
    /// stays, and keeps aging: it is not known to be wrong, only to be due.
    pub(crate) fn redo(&mut self) {
        self.current = None;
        self.want = true;
        self.asap = true;
    }

    /// The data was read longer ago than `limit`.
    pub(crate) fn aged(&self, mono: u64, limit: u64) -> bool {
        self.last_ok.is_some_and(|t| mono.saturating_sub(t) > limit)
    }
}
