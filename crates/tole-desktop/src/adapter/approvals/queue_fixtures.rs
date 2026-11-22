//! Shared by the queue tests: server rows, a known queue, a driver step.

use serde_json::{json, Value};
use tole_client::{ApprovalList, Event, Frame};
use tole_desktop_model::approvals::{ApprovalsAction, ApprovalsState, FetchId, Scope};

use super::*;

pub(super) const NOW: u64 = 1_700_000_000_500;

pub(super) fn list(rows: Vec<Value>) -> ApprovalList {
    serde_json::from_value(json!({ "approvals": rows })).expect("list")
}

pub(super) fn serve_row() -> Value {
    json!({
        "id": "apr-1", "sessionId": "ses-1", "tool": "write_file",
        "description": "path: notes.txt", "status": "pending",
        "createdAtMs": 1_700_000_000_000u64, "expiresInSeconds": 900,
        "summary": {"kind": "create_file", "target": "notes.txt", "effect": "adds",
                    "insideWorkspace": true}
    })
}

pub(super) fn child_row() -> Value {
    json!({
        "id": "dap-9", "source": "delegation", "status": "pending", "node": "helper-1",
        "tool": "bash", "display": "run: rm x", "expiresInSeconds": 870,
        "fingerprint": "fp-abc"
    })
}

/// Applies the actions, and the fetch a loss asks for; returns the id of the
/// fetch started (if any).
pub(super) fn apply_all(
    q: &mut ApprovalsState,
    ids: &mut FetchIds,
    actions: Vec<ApprovalsAction>,
) -> Option<FetchId> {
    let mut started = None;
    for a in actions {
        q.apply(a.clone());
        if let Some((id, start)) = follow_up(&a, ids) {
            q.apply(start);
            started = Some(id);
        }
    }
    started
}

/// A queue whose whole list is known: the serve row and the child row wait.
pub(super) fn known(ids: &mut FetchIds) -> ApprovalsState {
    let mut q = ApprovalsState::new();
    let (id, start) = ids.start(Scope::All).expect("id");
    q.apply(start);
    q.apply(snapshot(
        &list(vec![serve_row(), child_row()]),
        Scope::All,
        id,
        NOW,
    ));
    q
}

pub(super) fn undecoded(event: &str, data: Value) -> Event {
    let frame = Frame {
        id: None,
        event: event.to_owned(),
        data: data.to_string(),
    };
    Event::from_frame(&frame).expect("a frame")
}
