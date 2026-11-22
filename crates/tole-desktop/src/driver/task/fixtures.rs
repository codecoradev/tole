//! Values the tests feed the driver: client types built from the wire JSON,
//! through the client's own decoders, as a serve would send them.

use serde_json::{json, Value};
use tole_client::{
    Approval, ApprovalList, CancelReply, DecisionReply, EntriesPage, Entry, Error, Event, Frame,
    InterruptReply, PromptReply, SessionStatus, StreamItem,
};

pub(super) const S: &str = "s1";

/// The client value a wire JSON decodes to (the type is the caller's).
macro_rules! from {
    ($v:expr) => {
        serde_json::from_value($v).expect("a client value")
    };
}

pub(super) fn entry(seq: u64, id: &str, parent: Option<&str>, kind: &str, payload: Value) -> Entry {
    from!(
        json!({"seq": seq, "id": id, "parentId": parent, "type": kind,
        "timestamp": 0, "payload": payload})
    )
}

pub(super) fn user(seq: u64, text: &str) -> Entry {
    let payload = json!({"role": "user", "text": text});
    entry(seq, &format!("e{seq}"), None, "message", payload)
}

pub(super) fn answer(seq: u64, text: &str) -> Entry {
    let payload = json!({"role": "assistant", "text": text});
    entry(seq, &format!("e{seq}"), None, "message", payload)
}

/// A page cut after `after` holding `entries`, as serve's `page_of` makes it.
pub(super) fn page(entries: Vec<Entry>, after: u64, has_more: bool, last_seq: u64) -> EntriesPage {
    let next_after = entries.last().map_or(after, |e| e.seq);
    from!(json!({"id": S, "entries": entries, "nextAfter": next_after,
        "hasMore": has_more, "lastSeq": last_seq}))
}

pub(super) fn status(busy: bool) -> SessionStatus {
    from!(
        json!({"id": S, "entries": null, "busy": busy, "usage_report": null,
        "model": null, "effort": null, "contextWindow": null, "contextUsed": null,
        "name": null, "snapshotAt": null, "stale": false})
    )
}

pub(super) fn cancelled(cancelled: bool) -> CancelReply {
    from!(json!({"cancelled": cancelled, "sessionId": S}))
}

pub(super) fn steered() -> InterruptReply {
    from!(json!({"queued": true, "urgent": false, "sessionId": S}))
}

pub(super) fn finished() -> PromptReply {
    from!(json!({"stopReason": "end_turn", "text": "ok"}))
}

pub(super) fn bare_approval_required() -> PromptReply {
    from!(json!({"stopReason": "approval_required"}))
}

pub(super) fn asking(id: &str) -> PromptReply {
    let row = approval(id, "pending");
    from!(json!({"stopReason": "approval_required", "approval": row}))
}

pub(super) fn approval(id: &str, status: &str) -> Approval {
    from!(json!({"id": id, "tool": "write_file", "status": status, "sessionId": S}))
}

pub(super) fn other_session_approval(id: &str) -> Approval {
    from!(json!({"id": id, "tool": "write_file", "status": "pending", "sessionId": "s2"}))
}

pub(super) fn list(rows: Vec<Approval>) -> ApprovalList {
    ApprovalList { approvals: rows }
}

pub(super) fn decided(id: &str, status: &str) -> DecisionReply {
    from!(json!({"id": id, "status": status}))
}

pub(super) fn status_code(status: u16) -> Error {
    Error::Status {
        status,
        message: "no".into(),
        body: Value::Null,
    }
}

pub(super) fn frame(name: &str, data: Value) -> StreamItem {
    let frame = Frame {
        id: None,
        event: name.into(),
        data: data.to_string(),
    };
    StreamItem::Event(Event::from_frame(&frame).expect("a frame"))
}

pub(super) fn entry_item(e: &Entry) -> StreamItem {
    frame("entry", serde_json::to_value(e).expect("json"))
}

pub(super) fn ready(busy: bool) -> StreamItem {
    frame(
        "ready",
        json!({"id": S, "after": 0, "lastSeq": 0, "busy": busy}),
    )
}

pub(super) fn delta(text: &str) -> StreamItem {
    frame("text_delta", json!({"text": text}))
}

pub(super) fn requested(id: &str) -> StreamItem {
    let data = json!({"id": id, "tool": "write_file", "status": "pending", "sessionId": S});
    frame("approval_requested", data)
}

pub(super) fn resolved(id: &str, status: &str) -> StreamItem {
    let data = json!({"id": id, "tool": "write_file", "status": status, "sessionId": S});
    frame("approval_resolved", data)
}

pub(super) fn reconnected() -> StreamItem {
    StreamItem::Reconnected { after: 0 }
}
