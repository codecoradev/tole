//! Request and response types of the serve routes a graphical client uses.
//!
//! Field names are the wire names: serve is not uniform (`stopReason` and
//! `sessionId` are camelCase, `plan_mode` and `usage_report` are snake_case),
//! and each rename below mirrors what the server emits. Every type both
//! serializes and deserializes, so the contract test can prove a response is
//! decoded without loss (re-serialized JSON equals the server's JSON), and the
//! committed JSON Schema is derived from these types (`schema/`, #1068).

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

pub use tole_core::entry::Entry;

macro_rules! wire {
    ($($item:item)*) => {$(
        #[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
        #[cfg_attr(test, derive(schemars::JsonSchema))]
        $item
    )*};
}

wire! {
    /// `GET /health`: `status`, and since serve reports its protocol and
    /// binary version (#1043) also `protocol` and `version`. Both are
    /// optional so a server that predates them still decodes, and a decoded
    /// answer serializes back to the JSON the server sent.
    pub struct Health {
        pub status: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub protocol: Option<u32>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub version: Option<String>,
    }

    /// One live session in `GET /sessions`.
    pub struct SessionSummary {
        pub id: String,
        pub busy: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub profile: Option<String>,
    }

    /// `GET /sessions`: the sessions live in this serve.
    pub struct SessionList {
        pub sessions: Vec<SessionSummary>,
    }

    /// One row of `GET /sessions/index`: a past or live session of the workspace.
    #[serde(rename_all = "camelCase")]
    pub struct PastSession {
        pub id: String,
        pub name: Option<String>,
        pub folder: String,
        /// Milliseconds since the Unix epoch.
        pub last_activity: u64,
        pub entries: u64,
        pub turns: u64,
        /// The session this one was forked from.
        pub parent: Option<String>,
        pub live: bool,
        pub busy: bool,
    }

    /// `GET /sessions/index`. Page with `cursor = next_cursor`.
    #[serde(rename_all = "camelCase")]
    pub struct SessionIndex {
        pub sessions: Vec<PastSession>,
        pub next_cursor: Option<String>,
        pub skipped: u64,
        pub scan_truncated: bool,
    }

    /// `POST /sessions`: create a session, or reopen one with `resume`.
    #[derive(Default)]
    pub struct NewSession {
        /// Working folder (must lie inside the server's workspace).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub cwd: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub profile: Option<String>,
        /// Only when creating: the registry without mutation tools.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub plan_mode: Option<bool>,
        /// Reopen this session id instead of creating one.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub resume: Option<String>,
    }

    /// The answer to `POST /sessions`. A create sets `workspace`; a resume
    /// sets `resumed` and `already_live` (and `busy` for a session that was
    /// already live).
    #[serde(rename_all = "camelCase")]
    pub struct SessionOpened {
        pub session_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub workspace: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub resumed: Option<bool>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub already_live: Option<bool>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub busy: Option<bool>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub plan_mode: Option<bool>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub profile: Option<String>,
    }

    /// `POST /sessions/{id}/prompt`.
    pub struct PromptRequest {
        pub text: String,
        /// Ids of images uploaded to this session.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        pub images: Vec<String>,
    }

    /// The answer to a prompt, once the turn stopped. `stop_reason` is
    /// `approval_required` when a Write waits for a decision: `approval` is
    /// then the queue row (absent when it was decided or expired meanwhile).
    #[serde(rename_all = "camelCase")]
    pub struct PromptReply {
        pub stop_reason: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub text: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub approval: Option<Approval>,
    }

    /// `POST /sessions/{id}/cancel`.
    #[serde(rename_all = "camelCase")]
    pub struct CancelReply {
        pub cancelled: bool,
        pub session_id: String,
    }

    /// `GET /sessions/{id}/status`. Model, effort and context figures are
    /// `null` until known; `stale` says they come from the last snapshot
    /// because a turn is running.
    #[serde(rename_all = "camelCase")]
    pub struct SessionStatus {
        pub id: String,
        /// `null` while a turn holds the session.
        pub entries: Option<u64>,
        pub busy: bool,
        /// The usage report as the server builds it (opaque to the client).
        #[serde(rename = "usage_report")]
        pub usage_report: Value,
        pub model: Option<String>,
        pub effort: Option<String>,
        pub context_window: Option<u64>,
        pub context_used: Option<u64>,
        pub name: Option<String>,
        pub snapshot_at: Option<u64>,
        pub stale: bool,
    }

    /// `GET /sessions/{id}/entries`. Page with `after = next_after` while
    /// `has_more`.
    #[serde(rename_all = "camelCase")]
    pub struct EntriesPage {
        pub id: String,
        #[cfg_attr(test, schemars(with = "Vec<crate::schema::EntrySchema>"))]
        pub entries: Vec<Entry>,
        pub next_after: u64,
        pub has_more: bool,
        pub last_seq: u64,
    }

    /// One row of the approval queue (`GET /approvals`, the
    /// `approval_requested` / `approval_resolved` event frames, and a prompt
    /// that stopped for approval). Rows of a delegated child have a different
    /// field set: what the client does not model is kept in `extra`.
    #[serde(rename_all = "camelCase")]
    pub struct Approval {
        pub id: String,
        pub tool: String,
        /// `pending`, `approved`, `denied` or `expired`.
        pub status: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub session_id: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub description: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub created_at_ms: Option<u64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub expires_in_seconds: Option<u64>,
        /// What the request will do, derived by code next to the gate (#1041).
        /// Absent on a row from a server without it. Boxed so that the event
        /// type that carries an approval stays small.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub summary: Option<Box<ActionSummary>>,
        #[serde(flatten)]
        pub extra: Map<String, Value>,
    }

    /// The structured summary of an approval request (#1041). `kind` and
    /// `effect` are the server's codes (`create_file`, `adds`, ...) and stay
    /// strings: a client translates by code, and a code it does not know is
    /// kept, not rejected. Nothing in a summary is model text, and it never
    /// changes the risk tier.
    #[serde(rename_all = "camelCase")]
    pub struct ActionSummary {
        pub kind: String,
        /// A path relative to the working folder, a command line, or a tool
        /// name.
        pub target: String,
        pub effect: String,
        /// True only when the server confirmed the target is inside the
        /// working folder.
        #[serde(default)]
        pub inside_workspace: bool,
        /// True when `target` was cut.
        #[serde(default)]
        pub target_truncated: bool,
        /// Bounded raw detail: diff lines (`+` and `-`), the command, or the
        /// input.
        #[serde(default)]
        pub preview: Vec<String>,
        /// True when `preview` is not the whole of it.
        #[serde(default)]
        pub preview_truncated: bool,
    }

    /// `GET /approvals`.
    pub struct ApprovalList {
        pub approvals: Vec<Approval>,
    }

    /// The body of `POST /approvals/{id}/decision`. Build it with
    /// [`DecisionRequest::new`]: a delegated child's row (`dap-*` id) needs
    /// `confirm` (the id) for an allow, and its `fingerprint`.
    pub struct DecisionRequest {
        pub decision: Decision,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub confirm: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub fingerprint: Option<String>,
    }

    /// `POST /approvals/{id}/decision`. A repeat of the same verdict is
    /// idempotent; the opposite verdict is a 409 ([`crate::Error::Status`]).
    pub struct DecisionReply {
        pub id: String,
        pub status: String,
        /// `true` when the allowed call is being replayed in the background.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub resuming: Option<bool>,
    }

    /// `GET /models`: what the owner lets a client pick.
    pub struct Models {
        pub models: Vec<String>,
        pub default: Option<String>,
    }
}

/// The verdict of `POST /approvals/{id}/decision`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
#[serde(rename_all = "lowercase")]
pub enum Decision {
    Allow,
    Deny,
}

impl DecisionRequest {
    /// The body for `approval_id`. Serve's own rows need only the verdict; a
    /// delegated child's (`dap-*`) allow also carries `confirm` set to the id,
    /// and the row's `fingerprint` when it has one.
    pub fn new(approval_id: &str, decision: Decision, fingerprint: Option<&str>) -> Self {
        let delegated = approval_id.starts_with("dap-");
        Self {
            decision,
            confirm: (delegated && decision == Decision::Allow).then(|| approval_id.to_string()),
            fingerprint: fingerprint.filter(|_| delegated).map(str::to_string),
        }
    }
}

impl NewSession {
    /// Reopen an existing session.
    pub fn resume(id: impl Into<String>) -> Self {
        Self {
            resume: Some(id.into()),
            ..Self::default()
        }
    }
}

impl PromptRequest {
    pub fn text(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            images: Vec::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn an_approval_row_decodes_its_summary_and_keeps_an_unknown_code() {
        let row = json!({
            "id": "apr-1", "tool": "write_file", "status": "pending",
            "summary": {
                "kind": "from_a_newer_server", "target": "n.md", "effect": "adds",
                "insideWorkspace": true, "targetTruncated": false,
                "preview": ["+ hi"], "previewTruncated": false
            }
        });
        let a: Approval = serde_json::from_value(row.clone()).unwrap();
        let s = a.summary.as_ref().expect("a summary");
        assert_eq!(
            (s.kind.as_str(), s.target.as_str()),
            ("from_a_newer_server", "n.md")
        );
        assert!(s.inside_workspace && !s.target_truncated && !s.preview_truncated);
        assert_eq!(s.preview, vec!["+ hi".to_string()]);
        // The summary is a typed field now, not part of `extra`, and the row
        // serializes back to the JSON the server sent.
        assert!(a.extra.is_empty(), "{:?}", a.extra);
        assert_eq!(serde_json::to_value(&a).unwrap(), row);
    }

    #[test]
    fn an_approval_row_without_a_summary_has_none_and_no_key() {
        let row = json!({"id": "apr-1", "tool": "write_file", "status": "pending"});
        let a: Approval = serde_json::from_value(row.clone()).unwrap();
        assert!(a.summary.is_none());
        assert_eq!(serde_json::to_value(&a).unwrap(), row);
    }

    #[test]
    fn a_decision_body_carries_confirm_and_fingerprint_only_for_delegated_rows() {
        let body = |id, d, f| serde_json::to_value(DecisionRequest::new(id, d, f)).unwrap();
        // Serve's own queue: the verdict only (an unknown key would be noise).
        assert_eq!(
            body("apr-1", Decision::Allow, Some("fp")),
            json!({"decision": "allow"})
        );
        // A delegated child: the exact keys serve_delegation::decide_route takes.
        assert_eq!(
            body("dap-9", Decision::Allow, Some("fp")),
            json!({"decision": "allow", "confirm": "dap-9", "fingerprint": "fp"})
        );
        assert_eq!(
            body("dap-9", Decision::Allow, None),
            json!({"decision": "allow", "confirm": "dap-9"})
        );
        assert_eq!(
            body("dap-9", Decision::Deny, None),
            json!({"decision": "deny"})
        );
    }
}
