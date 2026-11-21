//! The model's mirror of one row of the serve approval queue.
//!
//! The adapter copies the fields from `Approval` of `tole-client` (the list,
//! the `approval_requested` and `approval_resolved` frames, a prompt that
//! stopped for approval): this crate names no tole crate, so it mirrors the row.
//! A delegated child's `dap-` row has other fields (a node, no summary): the
//! adapter maps what it can, and the card says where it came from.

use super::summary::ActionSummary;

/// A request as the queue lists it. Text fields are untrusted data.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApprovalRequest {
    pub id: String,
    /// The task it belongs to. Absent on a delegated child's row.
    pub session_id: Option<String>,
    pub tool: String,
    pub description: Option<String>,
    /// `pending`, `approved`, `denied` or `expired`.
    pub status: String,
    pub created_at_ms: Option<u64>,
    /// The whole wait of a serve row (900) or the time left of a child's row.
    pub expires_in_secs: Option<u64>,
    pub summary: Option<ActionSummary>,
    /// `ReadOnly`, `Write` or `Destructive`; serve's rows do not report it yet.
    pub risk: Option<String>,
    /// Reported by delegated rows only today.
    pub fingerprint: Option<String>,
    /// The helper a delegated child's request came from (its `node`).
    pub child_node: Option<String>,
    /// Notification destinations told (`webhook_out`); no row reports it yet.
    pub notified: Vec<String>,
}

impl ApprovalRequest {
    /// A row with only the fields every row has.
    pub fn new(id: &str, tool: &str, status: &str) -> Self {
        ApprovalRequest {
            id: id.to_owned(),
            session_id: None,
            tool: tool.to_owned(),
            description: None,
            status: status.to_owned(),
            created_at_ms: None,
            expires_in_secs: None,
            summary: None,
            risk: None,
            fingerprint: None,
            child_node: None,
            notified: Vec::new(),
        }
    }

    /// A delegated child's request (`dap-` id, or the adapter said so).
    pub fn is_child(&self) -> bool {
        self.child_node.is_some() || self.id.starts_with("dap-")
    }

    /// The summary, or the unknown-tool summary when the row has none.
    pub fn summary_or_unknown(&self) -> ActionSummary {
        (self.summary.clone())
            .unwrap_or_else(|| ActionSummary::unknown(&self.tool, self.description.as_deref()))
    }
}

/// How a decided request ended (the labels of the Decided tab).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    Allowed,
    NotAllowed,
    Expired,
    /// No longer waiting and the outcome is not known: the server forgot it or
    /// said something this version does not know. Never worded as allowed.
    Closed,
}

impl Outcome {
    /// How a request with this `status` ended; `None` while it is `pending`.
    /// Any text this version does not know is [`Outcome::Closed`]: never
    /// pending (so never offered for an answer) and never allowed.
    pub fn of_status(status: &str) -> Option<Outcome> {
        match status {
            "pending" => None,
            "approved" => Some(Outcome::Allowed),
            "denied" => Some(Outcome::NotAllowed),
            "expired" => Some(Outcome::Expired),
            _ => Some(Outcome::Closed),
        }
    }
}

/// The answer to one request. There is no third value: an answer is one time
/// only, and the first answer wins.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    AllowOnce,
    DontAllow,
}

/// What the adapter sends for an answer: `POST /approvals/{id}/decision`
/// (`Decision::Allow` or `Deny`, and the fingerprint a delegated child's row
/// needs; `DecisionRequest::new` of the client adds the `confirm` field).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecisionIntent {
    pub id: String,
    pub decision: Decision,
    pub fingerprint: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unknown_status_text_is_never_allowed_and_never_pending() {
        assert_eq!(Outcome::of_status("pending"), None);
        assert_eq!(Outcome::of_status("approved"), Some(Outcome::Allowed));
        assert_eq!(Outcome::of_status("denied"), Some(Outcome::NotAllowed));
        assert_eq!(Outcome::of_status("expired"), Some(Outcome::Expired));
        for code in ["", "Approved", "allowed", "approved "] {
            assert_eq!(Outcome::of_status(code), Some(Outcome::Closed));
        }
    }

    #[test]
    fn a_dap_id_is_a_child_request_and_no_summary_reads_as_unknown() {
        assert!(ApprovalRequest::new("dap-1", "t", "pending").is_child());
        assert!(!ApprovalRequest::new("apr-1", "t", "pending").is_child());
        let s = ApprovalRequest::new("a", "frobnicate", "pending").summary_or_unknown();
        assert_eq!(
            (s.kind.as_str(), s.target.as_str()),
            ("unknown", "frobnicate")
        );
    }
}
