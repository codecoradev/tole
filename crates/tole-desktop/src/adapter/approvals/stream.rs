//! The event stream's approval frames, and its end, as queue actions.
//!
//! A stream that is lost in any way is `ConnectionLost` for the queue, and
//! each of those must be followed by a fetch (see the module docs of
//! `approvals` and [`super::follow_up`]).

use serde_json::Value;
use tole_client::{Error, Event, StreamItem};
use tole_desktop_model::approvals::{ApprovalRequest, ApprovalsAction};

use super::{request, status_of};
use crate::adapter::wire::{json_text, whole_number};

/// The frame names the client turns into [`Event::Other`] when their data does
/// not decode into an approval row.
const REQUESTED: &str = "approval_requested";
const RESOLVED: &str = "approval_resolved";

/// What one decoded frame means for the queue; empty for a frame that is not
/// about approvals. Never drops an approval frame silently.
///
/// - `approval_requested` is `Requested` (it repeats after every reconnect; the
///   queue keeps the first row of an id).
/// - `approval_resolved` is `Resolved`.
/// - `resync` says the server dropped frames for this slow stream. An approval
///   frame is not durable, so one may be among them: it is a lost connection.
/// - `closed` says the session left the server and the stream ends: no
///   `approval_resolved` of that session (another client's answer, the
///   server's expiry) can arrive on it any more, so its requests would stay
///   answerable until the local 900 s clock. It is a loss, which costs one
///   `GET /approvals` and fails toward Unknown (the list of the other
///   sessions is refetched with it).
/// - An approval frame whose data the client could not decode into a row
///   arrives as [`Event::Other`]. A pending request must not vanish, so for
///   `approval_requested` the id, tool, status, session, description and the
///   two times are read from the raw data (see `raw_request`; a request with
///   no readable id is refused by the queue anyway), the queue shows what it
///   can (the card is the careful "a tool it cannot describe"), and a loss
///   follows: the list is not known, and `GET /approvals` fails to decode too
///   while that row is pending, so the queue stays Unknown and every box
///   `Checking` for as long as it is. That is the intended degraded mode.
///   For `approval_resolved` a row with a readable id and status is applied;
///   anything less is a loss.
///
/// A `ConnectionLost` in the answer needs the fetch of [`super::follow_up`]
/// after it is applied.
pub fn event_actions(event: &Event, now_ms: u64) -> Vec<ApprovalsAction> {
    match event {
        Event::ApprovalRequested(row) => vec![ApprovalsAction::Requested(request(row), now_ms)],
        Event::ApprovalResolved(row) => vec![ApprovalsAction::Resolved(request(row), now_ms)],
        Event::Resync { .. } | Event::Closed { .. } => vec![ApprovalsAction::ConnectionLost],
        Event::Other { event, data } if event == REQUESTED => {
            let mut out = Vec::with_capacity(2);
            if let Some(row) = raw_request(data) {
                out.push(ApprovalsAction::Requested(row, now_ms));
            }
            out.push(ApprovalsAction::ConnectionLost);
            out
        }
        Event::Other { event, data } if event == RESOLVED => {
            let stated = data.get("status").is_some_and(Value::is_string);
            match raw_request(data) {
                Some(row) if stated => vec![ApprovalsAction::Resolved(row, now_ms)],
                _ => vec![ApprovalsAction::ConnectionLost],
            }
        }
        _ => Vec::new(),
    }
}

/// [`event_actions`] for an item of the resuming stream. `Reconnected` is
/// `ConnectionLost`: an `approval_resolved` sent while the stream was down is
/// never replayed.
pub fn stream_actions(item: &StreamItem, now_ms: u64) -> Vec<ApprovalsAction> {
    match item {
        StreamItem::Event(event) => event_actions(event, now_ms),
        StreamItem::Reconnected { .. } => vec![ApprovalsAction::ConnectionLost],
        StreamItem::Heartbeat => Vec::new(),
    }
}

/// The stream ended with an error (`ReconnectExhausted`, a 401 or 404 on the
/// reconnect): the resuming stream is spent and yields nothing more, so no
/// approval frame will ever arrive again. Apply this before the worker gives
/// up, so the queue stops claiming that nothing waits; whoever restarts the
/// stream follows with the fetch of [`super::follow_up`]. Any error of the
/// stream is a loss, whatever it says.
pub fn stream_error(_error: &Error) -> ApprovalsAction {
    ApprovalsAction::ConnectionLost
}

/// The stream ended without an error and without a `closed` frame (the
/// iterator returned `None`): no approval frame will arrive again either, so
/// it is a loss like the others. A driver that sees the end of the stream for
/// any reason applies this once, then restarts the stream and follows with the
/// fetch of [`super::follow_up`].
pub fn stream_ended() -> ApprovalsAction {
    ApprovalsAction::ConnectionLost
}

/// The tool of a request whose tool cannot be read: the model's own code for
/// an action it cannot describe, so the card is the careful "a tool it cannot
/// describe", never an empty subject.
const UNKNOWN_TOOL: &str = "unknown";

/// A request from the raw data of a frame the client could not decode. Nothing
/// is guessed except that a frame with no readable status is `pending`; a
/// value that is there but of another type is its JSON text, so the owner sees
/// what was sent. The id must be a string (`None` without one), because it is
/// what is answered. `createdAtMs` and `expiresInSeconds` are kept when they
/// are non-negative numbers (a fraction rounded down), so the countdown is the
/// server's and not a fresh 15 minutes. With no readable tool the tool is
/// `unknown` and the description is the frame's whole data, unless the frame
/// has a readable description of its own.
fn raw_request(data: &Value) -> Option<ApprovalRequest> {
    let string = |key: &str| data.get(key).and_then(Value::as_str).map(str::to_owned);
    let text = |key: &str| data.get(key).and_then(json_text);
    let id = string("id")?;
    let status = string("status").unwrap_or_else(|| "pending".to_owned());
    let tool = text("tool");
    let mut r = ApprovalRequest::new(
        &id,
        tool.as_deref().unwrap_or(UNKNOWN_TOOL),
        &status_of(&id, &status),
    );
    r.session_id = string("sessionId");
    r.description = text("description").or_else(|| tool.is_none().then(|| data.to_string()));
    r.created_at_ms = whole_number(data, "createdAtMs");
    r.expires_in_secs = whole_number(data, "expiresInSeconds");
    Some(r)
}
