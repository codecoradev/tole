//! Fetch ids and the fetch actions of the queue contract.
//!
//! The queue trusts only the reply of the current fetch of a scope, so every
//! `GET /approvals` has an id: [`FetchIds::start`] picks it and returns the
//! `FetchStarted` action to apply BEFORE the call, and the reply (or its
//! failure) goes back with the same id through [`snapshot`] or [`failed`]. A
//! fetch that fails, times out or is cancelled must end in [`failed`], or the
//! queue stays waiting for a reply that never comes.
//!
//! The queue asks for a fetch of everything in two ways, and [`due`] reads
//! both: `ApprovalsState::is_stale` (a `409`, or room that freed while
//! requests over the cap are uncounted) and `ApprovalsState::needs_fetch` (the
//! queue gave up a fetch that had neither a reply nor a failure, or a reply
//! left something it could not account for). Both are false while a fetch of
//! everything runs. It does not ask after [`failed`] or a loss: after
//! [`failed`] the whole list is unknown again and [`follow_up`] has no clock,
//! so it gives nothing for a failure. The driver retries itself: after each
//! failed fetch of everything it waits for [`Backoff::failed`] (1 s, then
//! doubling up to 30 s), starts a new fetch with [`FetchIds::start`] and calls
//! `GET /approvals` again, until one succeeds ([`Backoff::succeeded`]). It
//! never gives up, because until a fetch succeeds every message box is
//! `Checking` (disabled) and the badge `Unknown`; the cap keeps that to at most
//! one call per 30 s. A loss ([`follow_up`]) starts a fetch at once and begins
//! a new sequence ([`Backoff::succeeded`] or a new `Backoff`).
//!
//! The driver must send `ApprovalsAction::Tick` at least every [`TICK_MAX`]
//! (5 s) for as long as the app runs, not only for the next request deadline:
//! the watchdog that gives up a fetch after `FETCH_TIMEOUT_SECS`, the deadlines
//! of refusals and the expiry of requests run only on a `Tick`. After each
//! applied action (a `Tick` included) it calls [`due`] and starts the fetch it
//! returns.

use std::time::Duration;

use tole_client::ApprovalList;
use tole_desktop_model::approvals::{ApprovalsAction, ApprovalsState, FetchId, Scope};

use super::requests;

/// The counter that makes fetch ids.
///
/// There must be exactly ONE per `ApprovalsState`, owned by whoever owns the
/// state and shared by every worker that fetches for it (behind the same lock
/// as the state): two counters would both give id 1 for `Scope::All`, and the
/// late reply of one worker's fetch would be taken for the current fetch of
/// the other. So it is not `Clone` and every method takes `&mut self`.
///
/// Ids never repeat. After `u64::MAX - 1` ids (never reached in practice) it
/// gives no more: [`next_id`](Self::next_id) and [`start`](Self::start) return
/// `None`, no fetch is started, and the queue stays unknown (badge `Unknown`,
/// message boxes `Checking`), the careful state.
#[derive(Debug, Default)]
pub struct FetchIds {
    last: u64,
}

impl FetchIds {
    pub fn new() -> Self {
        Self::default()
    }

    /// A counter that has already given out `last` (for the exhaustion test).
    #[cfg(test)]
    pub(super) fn after(last: u64) -> Self {
        Self { last }
    }

    /// The next id (the first is 1), or `None` when the counter is spent.
    pub fn next_id(&mut self) -> Option<FetchId> {
        let next = self.last.checked_add(1)?;
        self.last = next;
        Some(FetchId(next))
    }

    /// A new fetch of `scope`: its id and the `FetchStarted` action to apply
    /// before the call is made. `None` when the counter is spent.
    pub fn start(&mut self, scope: Scope) -> Option<(FetchId, ApprovalsAction)> {
        let id = self.next_id()?;
        Some((id, ApprovalsAction::FetchStarted(scope, id)))
    }
}

/// The longest gap between two `Tick` actions the driver may leave.
pub const TICK_MAX: Duration = Duration::from_secs(5);
/// The shortest wait before a retry of a failed fetch.
pub const RETRY_MIN: Duration = Duration::from_secs(1);
/// The longest wait before a retry: the delay stops growing here.
pub const RETRY_MAX: Duration = Duration::from_secs(30);

/// How long to wait before the next try of a fetch that keeps failing: a pure
/// counter (no clock, no timer; the driver sleeps). One per sequence of tries.
#[derive(Debug, Default)]
pub struct Backoff {
    failures: u32,
}

impl Backoff {
    pub fn new() -> Self {
        Self::default()
    }

    /// A sequence that already failed `failures` times (for the bound test).
    #[cfg(test)]
    pub(super) fn after(failures: u32) -> Self {
        Self { failures }
    }

    /// A fetch failed: how long to wait before the next one. [`RETRY_MIN`]
    /// after the first failure, doubling after each, at most [`RETRY_MAX`].
    pub fn failed(&mut self) -> Duration {
        self.failures = self.failures.saturating_add(1);
        let doublings = self.failures.saturating_sub(1);
        RETRY_MIN
            .saturating_mul(2u32.saturating_pow(doublings))
            .min(RETRY_MAX)
    }

    /// A fetch succeeded (or a loss started a fresh sequence): the next failure
    /// waits [`RETRY_MIN`] again.
    pub fn succeeded(&mut self) {
        self.failures = 0;
    }

    /// Failures since the last success.
    pub fn failures(&self) -> u32 {
        self.failures
    }
}

/// The fetch that must follow an action just applied: for `ConnectionLost`,
/// the `FetchStarted` of everything with its id (call `GET /approvals`, then
/// [`snapshot`] or [`failed`] with that id); for any other action, `None`.
///
/// The driver's loop for each action `a` from `stream_actions`, `stream_error`
/// or anywhere else: apply `a`, then `follow_up(&a, ids)` and, when it gives a
/// fetch, apply its action and make the call. A loss while a fetch runs needs a
/// new fetch too (the old reply is no longer trusted), so call it every time.
pub fn follow_up(
    action: &ApprovalsAction,
    ids: &mut FetchIds,
) -> Option<(FetchId, ApprovalsAction)> {
    match action {
        ApprovalsAction::ConnectionLost => ids.start(Scope::All),
        _ => None,
    }
}

/// The fetch the queue asks for by itself: a new fetch of everything when
/// `needs_fetch` or `is_stale` says so (both are false while one runs), with
/// the `FetchStarted` action to apply before the call; `None` when nothing is
/// asked, or the counter is spent. Call it after every applied action.
pub fn due(queue: &ApprovalsState, ids: &mut FetchIds) -> Option<(FetchId, ApprovalsAction)> {
    if queue.needs_fetch() || queue.is_stale() {
        return ids.start(Scope::All);
    }
    None
}

/// The `session` argument of `Client::approvals` for a scope.
pub fn scope_session(scope: &Scope) -> Option<&str> {
    match scope {
        Scope::All => None,
        Scope::Session(id) => Some(id),
    }
}

/// The reply of the fetch `id` of `scope`, as the `Snapshot` action.
pub fn snapshot(list: &ApprovalList, scope: Scope, id: FetchId, now_ms: u64) -> ApprovalsAction {
    ApprovalsAction::Snapshot(requests(list), scope, id, now_ms)
}

/// The fetch `id` of `scope` failed, timed out or was cancelled.
pub fn failed(scope: Scope, id: FetchId) -> ApprovalsAction {
    ApprovalsAction::FetchFailed(scope, id)
}
