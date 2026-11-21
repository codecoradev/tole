//! The model's answer to the client's request.

use tole_client::{Approval, Decision as Verdict, DecisionRequest};
use tole_desktop_model::approvals::{Decision, DecisionIntent};

/// The verdict. The model has two values, one time only: there is no "always".
pub fn decision_to_client(decision: Decision) -> Verdict {
    match decision {
        Decision::AllowOnce => Verdict::Allow,
        Decision::DontAllow => Verdict::Deny,
    }
}

/// The body of `POST /approvals/{id}/decision` for an intent. The client's own
/// constructor decides which keys go on the wire: the verdict only for serve's
/// rows, and for a delegated child's (`dap-` id) allow also `confirm` (the id)
/// and the `fingerprint` of the row; a child's deny carries the fingerprint
/// only (no `confirm`).
pub fn decision_request(intent: &DecisionIntent) -> DecisionRequest {
    DecisionRequest::new(
        &intent.id,
        decision_to_client(intent.decision),
        intent.fingerprint.as_deref(),
    )
}

/// A minimal row to hand to `Client::decide_row`.
///
/// The client's public decision calls are `decide(id, verdict)`, which cannot
/// carry a fingerprint, and `decide_row(&Approval, verdict)`, which takes the
/// fingerprint from the row's `extra`. A delegated child's allow needs it, and
/// the model's intent holds only the id and the fingerprint, so this builds the
/// row `decide_row` reads: the id, a `pending` status, and the fingerprint when
/// there is one. Use it for every intent; for serve's own rows the extra key is
/// ignored by `DecisionRequest::new`.
pub fn row_for_decision(intent: &DecisionIntent) -> Approval {
    let mut row = Approval {
        id: intent.id.clone(),
        tool: String::new(),
        status: "pending".to_owned(),
        session_id: None,
        description: None,
        created_at_ms: None,
        expires_in_seconds: None,
        summary: None,
        extra: Default::default(),
    };
    if let Some(fp) = &intent.fingerprint {
        row.extra
            .insert("fingerprint".to_owned(), fp.clone().into());
    }
    row
}
