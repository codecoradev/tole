//! Approvals: the client's queue rows and frames to the model's
//! [`ApprovalRequest`], and the model's answer back to the client.
//!
//! This is the adapter contract written in the model's queue module:
//!
//! - `request` maps one `Approval` row (`GET /approvals`, the
//!   `approval_requested` and `approval_resolved` frames, the row of a prompt
//!   that stopped for approval) field for field, with the action summary of
//!   #1041 (`summary`). Text is raw; the queue bounds and checks it.
//! - A delegated child's row has a `dap-` id (one rule, `is_child_id`, for
//!   the model's reading and for what is sent back) and other fields, which
//!   the client keeps in `extra`: the `node` it came from (`child_node`), its
//!   `fingerprint`, and `display` (the one line the owner sees at a terminal)
//!   as the description, since it has no `description`. It has no summary, so
//!   the card falls back to the unknown-tool summary. `goal`, `target`, `text`,
//!   `chain` and `ageSeconds` have no field in the model and are not mapped.
//!   Serve's decision route answers a child's decision with `allowed` or
//!   `denied` (`serve_delegation::decide_route`), where the model knows
//!   `approved` and `denied`: [`status_of`] translates `allowed` for a child.
//!   Serve's own queue says `approved`, `denied`, `expired` and needs no
//!   translation.
//! - `fetch`: the `FetchIds` counter, the three fetch actions and `follow_up`,
//!   the fetch that must follow every loss of the stream.
//! - `stream`: the stream's items and errors as actions.
//! - `decision`: an answer as the request the client sends.
//!
//! The one thing a driver must do after the stream was lost, in any of its four
//! forms (`Reconnected`, `resync`, a frame that could not be read, the stream
//! ending with an error): apply the `ConnectionLost` action, then start a fetch
//! of everything (`follow_up` returns its `FetchStarted`), call
//! `GET /approvals` and apply `snapshot` or `failed` with that id. The model
//! cannot ask for it (`ApprovalsState::is_stale` stays false after a loss), and
//! until a fetch succeeds the badge is `Unknown` and the message box of every
//! task is `Checking`. A loss while a fetch runs makes that fetch's reply
//! untrusted: start a new one. A fetch that fails is retried by the driver with
//! `Backoff` (the queue never asks again; see `fetch`).

mod decision;
mod fetch;
#[cfg(test)]
mod frame_tests;
#[cfg(test)]
mod queue_fixtures;
#[cfg(test)]
mod queue_tests;
mod stream;
mod summary;
#[cfg(test)]
mod tests;

use tole_client::{Approval, ApprovalList, DecisionReply};
use tole_desktop_model::approvals::ApprovalRequest;

use super::wire::extra_text;

pub use decision::{decision_request, decision_to_client, row_for_decision};
pub use fetch::{
    failed, follow_up, scope_session, snapshot, Backoff, FetchIds, RETRY_MAX, RETRY_MIN,
};
pub use stream::{event_actions, stream_actions, stream_ended, stream_error};
pub use summary::summary;

/// The id prefix of a delegated child's row.
const CHILD_ID_PREFIX: &str = "dap-";

/// Whether an approval id is a delegated child's. The one rule: it is the
/// client's (`DecisionRequest::new` sends `confirm` and the fingerprint for
/// exactly these ids), so what the model shows as a helper's request is what
/// is answered as one. A row that only says `source: "delegation"` under
/// another id is not a child: it is shown and answered as the serve row its id
/// names.
fn is_child_id(id: &str) -> bool {
    id.starts_with(CHILD_ID_PREFIX)
}

/// The status as the model knows it. A delegated child's decision route says
/// `allowed` where the model's word is `approved`; every other text is passed
/// on, and the model reads one it does not know as closed.
pub fn status_of(id: &str, status: &str) -> String {
    match status {
        "allowed" if is_child_id(id) => "approved".to_owned(),
        other => other.to_owned(),
    }
}

/// One queue row as the model's request. Every field is the server's own
/// value; a field the row does not have is `None`.
///
/// `risk` is read from the row's unmodelled `risk` key when the server ever
/// sends one (serve's rows do not report it yet); the model only ever lets it
/// raise the care of a card. A `risk` that is not a string reaches the model as
/// its JSON text, an unknown code read as a danger, never as "not reported".
/// `notified` stays empty: no row reports it.
pub fn request(row: &Approval) -> ApprovalRequest {
    let child = is_child_id(&row.id);
    ApprovalRequest {
        id: row.id.clone(),
        session_id: row.session_id.clone(),
        tool: row.tool.clone(),
        // A child's row has no description; its `display` line is the same
        // text the owner reads at a terminal (and is redacted by the server).
        description: row
            .description
            .clone()
            .or_else(|| child.then(|| extra_text(row, "display")).flatten()),
        status: status_of(&row.id, &row.status),
        created_at_ms: row.created_at_ms,
        expires_in_secs: row.expires_in_seconds,
        summary: row.summary.as_deref().map(summary),
        risk: extra_text(row, "risk"),
        fingerprint: extra_text(row, "fingerprint"),
        // A child with no usable `node` still says it is a helper's request
        // (an empty origin reads "From a helper Tole started").
        child_node: child.then(|| extra_text(row, "node").unwrap_or_default()),
        notified: Vec::new(),
    }
}

/// Every row of a `GET /approvals` answer, in the server's order.
pub fn requests(list: &ApprovalList) -> Vec<ApprovalRequest> {
    list.approvals.iter().map(request).collect()
}

/// The reply of a decision call as the request for
/// `ApprovalsAction::Resolved`. The reply names only the id and the new status
/// (`approved` or `denied` from serve's queue, `allowed` or `denied` for a
/// delegated child, which [`status_of`] translates), so the rest is empty: the
/// queue keeps the row
/// it already has and reads only the status of this one. `resuming` has no
/// field in the model and is not mapped.
pub fn resolved_from_reply(reply: &DecisionReply) -> ApprovalRequest {
    ApprovalRequest::new(&reply.id, "", &status_of(&reply.id, &reply.status))
}
