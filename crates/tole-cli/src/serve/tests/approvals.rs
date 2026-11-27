//! Tests of the approval routes: the decision audit and the resume of an evicted session.

use super::support::*;
use crate::serve::*;

/// A first decision whose durable audit write failed (the client got a
/// 5xx) is finished by a retry of the same verdict: the audit is written
/// and the resume starts then, once. The opposite verdict is still a
/// conflict, and once the audit is in, a repeat is a plain 200.
#[test]
fn a_decision_whose_audit_failed_is_finished_by_its_retry() {
    let base = crate::test_dir::TestDir::new("tole-serve-audit-retry");
    let root = base.join("ws");
    let sessions = base.join("sessions");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::create_dir_all(&sessions).unwrap();
    let state = test_state(root, sessions);
    // The session file's directory does not exist yet: the audit cannot
    // be written.
    let missing = base.join("later");
    let id = state.approvals.queue_entry(
        "srv-audit-retry",
        "write_file",
        "write file x.txt",
        7,
        missing.join("srv-audit-retry.jsonl"),
    );
    let path = format!("/approvals/{id}/decision");
    let allow = json!({"decision": "allow"}).to_string();
    let deny = json!({"decision": "deny"}).to_string();

    let (status, body) = route(&state, "POST", &path, &allow);
    assert_eq!(status, 500, "{body}");
    // The verdict took effect in the queue; the opposite one conflicts.
    let (status, body) = route(&state, "POST", &path, &deny);
    assert_eq!(status, 409, "{body}");
    // A retry while the audit still cannot be written fails again, and
    // does not claim to be a repeat.
    let (status, body) = route(&state, "POST", &path, &allow);
    assert_eq!(status, 500, "{body}");
    // The audit can be written now: the retry finishes the decision.
    std::fs::create_dir_all(&missing).unwrap();
    tole_core::storage::JsonlStorage::create(&missing, "srv-audit-retry", None).unwrap();
    // The session is not live here, so after the audit nothing resumes:
    // the answer says so (#979) instead of `resuming: true`.
    let (status, body) = route(&state, "POST", &path, &allow);
    assert_eq!(status, 409, "{body}");
    assert_eq!(body["resuming"], false);
    let audit = std::fs::read_to_string(missing.join("srv-audit-retry.jsonl")).unwrap();
    assert_eq!(
        audit.matches(&format!("approval_{id}")).count(),
        1,
        "{audit}"
    );
    // Done: a further repeat is idempotent and writes nothing.
    let (status, body) = route(&state, "POST", &path, &allow);
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["resuming"], false);
    let audit = std::fs::read_to_string(missing.join("srv-audit-retry.jsonl")).unwrap();
    assert_eq!(
        audit.matches(&format!("approval_{id}")).count(),
        1,
        "{audit}"
    );
}

/// #979: an Allow for a session this server no longer hosts, with no
/// parked cron run, must not answer `resuming: true`; a Deny is still a
/// plain recorded verdict.
#[test]
fn an_allow_for_a_session_that_is_not_live_is_refused_not_promised() {
    let base = crate::test_dir::TestDir::new("tole-serve-evicted-allow");
    let root = base.join("ws");
    let sessions = base.join("sessions");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::create_dir_all(&sessions).unwrap();
    let state = test_state(root, sessions.clone());
    tole_core::storage::JsonlStorage::create(&sessions, "srv-gone", None).unwrap();
    let queue = |n: u64| {
        state.approvals.queue_entry(
            "srv-gone",
            "write_file",
            "write file x.txt",
            n,
            sessions.join("srv-gone.jsonl"),
        )
    };
    let id = queue(1);
    let (status, body) = route(
        &state,
        "POST",
        &format!("/approvals/{id}/decision"),
        &json!({"decision": "allow"}).to_string(),
    );
    assert_eq!(status, 409, "{body}");
    assert_eq!(body["resuming"], false);
    assert!(body["error"].as_str().unwrap().contains("not live"));
    // The refused allow left no one-shot behind (F2).
    assert!(!state.approvals.has_pending("srv-gone"));
    let id = queue(2);
    let (status, body) = route(
        &state,
        "POST",
        &format!("/approvals/{id}/decision"),
        &json!({"decision": "deny"}).to_string(),
    );
    assert_eq!(status, 200, "{body}");
}
