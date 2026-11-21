//! Approval rows to events.
//!
//! - `approval_requested` is [`TaskEvent::ApprovalRequested`]. A row that already
//!   says it was decided (`approved`, `denied`, `expired`, `closed`), as serve's
//!   poll sends for a request queued and decided between two passes, is the
//!   pair `ApprovalRequested` then `ApprovalResolved`: the card appears and goes
//!   at once, and an allow still moves the model to `Resuming` (it needs the
//!   approval to have been held). A status this client does not know is shown
//!   (the server has the last word on a decision).
//! - `approval_resolved` is [`TaskEvent::ApprovalResolved`] with `approved` only
//!   for the status `approved`; any other status, `pending` included, is a
//!   denial (nothing is allowed on a doubt).
//! - The queue is [`TaskEvent::ApprovalsRefreshed`] with the rows that are not
//!   decided and whose `sessionId` is this task's. A row with no session is
//!   dropped: `GET /approvals` without `?session=` appends the durable queue of
//!   delegated children (`dap-*` rows, `serve_delegation::list`), which no serve
//!   session owns and whose decision never reaches a task's stream; they belong
//!   to the Approvals page. Ask with `Client::approvals(Some(session))` (serve
//!   then leaves the children out) and the filter here is the second guard.
//!   More than [`MAX_REFRESH_ROWS`] rows for one task is not a queue: it is
//!   refused (`None`), the model keeps `approvals_stale`, and the driver retries
//!   later instead of claiming a complete list. A failed fetch is the same: send
//!   nothing, and retry while `run.approvals_stale`, spaced by
//!   [`approvals_backoff`] (2 s doubling to 60 s, [`MAX_APPROVALS_RETRIES`] tries).
//!   The first fetch after the stream opens is not covered by `approvals_stale`
//!   (only `Reconnected` sets it), but serve's own watch sends every pending
//!   request as a frame right after `ready`, so a failed first fetch loses
//!   nothing the stream does not repeat.
//! - Live `approval_requested` frames are bounded by the mapper, not the model:
//!   the model keeps `run.approvals` without a count cap (only its dedupe
//!   memory has one). [`EntryMapper`](super::EntryMapper) admits at most
//!   [`MAX_REFRESH_ROWS`] open approval ids (a repeat of one is passed on, the
//!   model dedupes it; a new one over the cap, or an id over 256 bytes, is
//!   dropped), frees an id on its `approval_resolved`, and starts over on a
//!   `Reconnected` (the refresh that follows lists what is really pending).
//! - The text of an approval is the server's `description` when it has one,
//!   else the target of the structured summary; raw, the model cleans it.

use std::time::Duration;
use tole_client::Approval;
use tole_desktop_model::task::{ApprovalInfo, TaskEvent};

/// Most rows of one task's queue taken as a queue (a task asks few approvals).
pub const MAX_REFRESH_ROWS: usize = 500;

/// First wait before a failed or refused queue fetch is tried again.
pub const APPROVALS_RETRY_BASE: Duration = Duration::from_secs(2);
/// Longest wait between two such tries.
pub const APPROVALS_RETRY_MAX: Duration = Duration::from_secs(60);
/// Tries after which the driver stops asking (the stream still announces).
pub const MAX_APPROVALS_RETRIES: u32 = 8;

/// The wait before retry number `attempt` (0 is the first retry) of the queue
/// fetch: [`APPROVALS_RETRY_BASE`] doubling up to [`APPROVALS_RETRY_MAX`], and
/// `None` once [`MAX_APPROVALS_RETRIES`] tries were made.
pub fn approvals_backoff(attempt: u32) -> Option<Duration> {
    if attempt >= MAX_APPROVALS_RETRIES {
        return None;
    }
    let wait = APPROVALS_RETRY_BASE.saturating_mul(1u32.checked_shl(attempt).unwrap_or(u32::MAX));
    Some(wait.min(APPROVALS_RETRY_MAX))
}

/// The statuses that mean the request has been decided.
pub(super) fn decided(status: &str) -> bool {
    matches!(status, "approved" | "denied" | "expired" | "closed")
}

/// One row as the model's approval.
pub fn approval_info(a: &Approval) -> ApprovalInfo {
    let described = a.description.as_deref().filter(|d| !d.is_empty());
    let target = a.summary.as_ref().map(|s| s.target.as_str());
    ApprovalInfo {
        id: a.id.clone(),
        tool: a.tool.clone(),
        summary: described.or(target).map(str::to_string),
    }
}

/// A row of an `approval_requested` frame: the request, then its resolution
/// when the row says it was decided already.
pub fn approval_requested(a: &Approval) -> Vec<TaskEvent> {
    let mut events = vec![TaskEvent::ApprovalRequested(approval_info(a))];
    if decided(&a.status) {
        events.push(approval_resolved(a));
    }
    events
}

/// The `approval` of a prompt reply. The reply ends the turn that asked, so a
/// row decided meanwhile has its resolution after `TurnEnded`: the turn the
/// allow resumed must not be ended by the reply that preceded it.
pub fn reply_approval(a: &Approval) -> Vec<TaskEvent> {
    let mut events = approval_requested(a);
    let resolved = events.split_off(1);
    events.push(TaskEvent::TurnEnded);
    events.extend(resolved);
    events
}

/// A row of an `approval_resolved` frame.
pub fn approval_resolved(a: &Approval) -> TaskEvent {
    TaskEvent::ApprovalResolved {
        id: a.id.clone(),
        approved: a.status == "approved",
    }
}

/// The queue as the server holds it for `session`. Send it once after opening
/// the stream, and again after every `Reconnected`. `None` for a list over
/// [`MAX_REFRESH_ROWS`] (see the module docs).
pub fn approvals_refreshed(rows: &[Approval], session: &str) -> Option<TaskEvent> {
    let mine = rows
        .iter()
        .filter(|a| !decided(&a.status))
        .filter(|a| a.session_id.as_deref() == Some(session));
    let pending: Vec<_> = mine.take(MAX_REFRESH_ROWS + 1).map(approval_info).collect();
    (pending.len() <= MAX_REFRESH_ROWS).then_some(TaskEvent::ApprovalsRefreshed { pending })
}
