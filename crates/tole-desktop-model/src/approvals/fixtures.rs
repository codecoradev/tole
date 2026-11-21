//! Fixtures shared by the tests of the card and of the queue.

use super::*;

pub const T0: u64 = 1_000_000_000;

pub fn req(id: &str, session: &str, kind: &str, effect: &str) -> ApprovalRequest {
    let mut r = ApprovalRequest::new(id, "write_file", "pending");
    r.session_id = Some(session.into());
    r.created_at_ms = Some(T0);
    r.expires_in_secs = Some(900);
    r.summary = Some(ActionSummary {
        kind: kind.into(),
        target: "Weekly report.txt".into(),
        effect: effect.into(),
        inside_workspace: true,
        target_truncated: false,
        preview: vec!["+ one".into(), "+ two".into()],
        preview_truncated: false,
    });
    r
}
