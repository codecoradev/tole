//! The schedule of one tab's loads: when the next call may start and what each
//! outcome does to it. Pure bookkeeping on the monotonic clock; the page and
//! the replies are the engine's (`loading`).

use std::time::Duration;

use super::millis;
use crate::adapter::approvals::{Backoff, RETRY_MIN};
use crate::adapter::skills::LoadId;

pub(super) struct Lane {
    /// The call the page trusts: the one started last, not lost since.
    pub current: Option<LoadId>,
    /// The call whose answer is awaited (also after it was lost: its thread is
    /// still in it). The one slot of the route: only its answer frees it.
    pub out: Option<LoadId>,
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

impl Lane {
    /// Nothing is loaded: the first call is wanted at once.
    pub(super) fn new() -> Self {
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

    pub(super) fn may_start(&self, mono: u64) -> bool {
        self.want
            && self.out.is_none()
            && mono >= self.floor
            && (mono >= self.not_before || self.asap)
    }

    pub(super) fn started(&mut self, id: LoadId, mono: u64) {
        self.want = false;
        self.asap = false;
        self.out = Some(id);
        self.current = Some(id);
        self.floor = mono.saturating_add(millis(RETRY_MIN));
    }

    /// A refresh is due, or the owner asked for one, and nothing trusted is
    /// out: a load is wanted.
    pub(super) fn schedule(&mut self, mono: u64) {
        let due = self.next_refresh.is_some_and(|t| mono >= t);
        if self.current.is_none() && (self.asap || due) {
            self.want = true;
            self.next_refresh = None;
        }
    }

    /// The call of `current` answered with data that is on the page now.
    pub(super) fn succeeded(&mut self, mono: u64, refresh: Duration) {
        self.current = None;
        self.want = false;
        self.backoff.succeeded();
        self.not_before = 0;
        self.last_ok = Some(mono);
        self.next_refresh = Some(mono.saturating_add(millis(refresh)));
    }

    /// The call of `current` failed (or its answer could not be used): retry
    /// after the next backoff step, for ever.
    pub(super) fn failed(&mut self, mono: u64) {
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
    pub(super) fn lost(&mut self, drop_call: bool) {
        if drop_call {
            self.current = None;
        }
        self.last_ok = None;
        self.next_refresh = None;
        if self.current.is_none() {
            self.want = true;
        }
    }

    /// The data was read longer ago than `limit`.
    pub(super) fn aged(&self, mono: u64, limit: u64) -> bool {
        self.last_ok.is_some_and(|t| mono.saturating_sub(t) > limit)
    }
}
