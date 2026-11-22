//! The mapping, from rows built out of the server's own JSON.

use serde_json::{json, Value};
use tole_client::{Approval, ApprovalList, DecisionReply};
use tole_desktop_model::approvals::{
    risk_care, ApprovalsAction, ApprovalsState, Care, Decision, DecisionIntent, FetchId, Scope,
};

use super::*;

fn row(v: Value) -> Approval {
    serde_json::from_value(v).expect("a row the client decodes")
}

/// A row as `approval_json` of the serve builds it, with the summary of #1041.
fn serve_row() -> Value {
    json!({
        "id": "apr-1", "sessionId": "ses-1", "tool": "write_file",
        "description": "path: notes.txt", "status": "pending",
        "createdAtMs": 1_700_000_000_000u64, "expiresInSeconds": 900,
        "summary": {
            "kind": "create_file", "target": "notes.txt", "effect": "adds",
            "insideWorkspace": true, "targetTruncated": false,
            "preview": ["+ hello", "+ world"], "previewTruncated": true
        }
    })
}

/// A row as `serve_delegation::list` builds it.
fn child_row() -> Value {
    json!({
        "id": "dap-9", "source": "delegation", "status": "pending",
        "goal": "tidy", "node": "helper-1", "tool": "bash",
        "target": "rm x", "display": "run: rm x", "ageSeconds": 30,
        "expiresInSeconds": 870, "fingerprint": "fp-abc",
        "chain": [{"level": 1, "node": "root", "depth": 0, "skipped": null}],
        "text": "request dap-9\n~~~\nrun: rm x\n~~~"
    })
}

#[test]
fn a_serve_row_maps_field_for_field() {
    let r = request(&row(serve_row()));
    assert_eq!(
        (r.id.as_str(), r.session_id.as_deref(), r.tool.as_str()),
        ("apr-1", Some("ses-1"), "write_file")
    );
    assert_eq!(r.description.as_deref(), Some("path: notes.txt"));
    assert_eq!(r.status, "pending");
    assert_eq!(r.created_at_ms, Some(1_700_000_000_000));
    assert_eq!(r.expires_in_secs, Some(900));
    let s = r.summary.clone().expect("summary");
    assert_eq!(
        (s.kind.as_str(), s.target.as_str(), s.effect.as_str()),
        ("create_file", "notes.txt", "adds")
    );
    assert!(s.inside_workspace && !s.target_truncated && s.preview_truncated);
    assert_eq!(s.preview, ["+ hello", "+ world"]);
    assert_eq!(
        (&r.risk, &r.fingerprint, &r.child_node),
        (&None, &None, &None)
    );
    assert!(r.notified.is_empty());
    assert!(!r.is_child(), "an apr- row is a serve row");
}

#[test]
fn a_row_without_a_summary_has_none_and_the_model_falls_back() {
    let r = request(&row(
        json!({"id": "apr-2", "tool": "frobnicate", "status": "pending"}),
    ));
    assert!(r.summary.is_none() && r.session_id.is_none() && r.description.is_none());
    let s = r.summary_or_unknown();
    assert_eq!(
        (s.kind.as_str(), s.target.as_str()),
        ("unknown", "frobnicate")
    );
}

#[test]
fn a_summary_missing_its_flags_reads_as_outside_and_whole() {
    let r = request(&row(json!({
        "id": "a", "tool": "write_file", "status": "pending",
        "summary": {"kind": "create_file", "target": "x", "effect": "adds"}
    })));
    let s = r.summary.expect("summary");
    assert!(!s.inside_workspace, "not confirmed inside: outside");
    assert!(s.preview.is_empty() && !s.preview_truncated);
}

#[test]
fn a_delegated_child_row_carries_node_fingerprint_and_display() {
    let r = request(&row(child_row()));
    assert!(r.is_child());
    assert_eq!(r.child_node.as_deref(), Some("helper-1"));
    assert_eq!(r.fingerprint.as_deref(), Some("fp-abc"));
    assert_eq!(r.description.as_deref(), Some("run: rm x"));
    assert_eq!(r.expires_in_secs, Some(870), "the time left of the child");
    assert!(r.session_id.is_none() && r.summary.is_none() && r.created_at_ms.is_none());
    let s = r.summary_or_unknown();
    assert_eq!((s.kind.as_str(), s.target.as_str()), ("unknown", "bash"));
    assert_eq!(s.preview, ["run: rm x"]);
}

#[test]
fn one_rule_decides_what_a_child_is() {
    // The id says so: the model and the request that is sent agree.
    let by_id = request(&row(
        json!({"id": "dap-1", "tool": "t", "status": "pending"}),
    ));
    assert!(by_id.is_child());
    assert_eq!(
        by_id.child_node.as_deref(),
        Some(""),
        "no node: empty origin"
    );
    // `source: "delegation"` under another id is a serve row for both: the
    // client sends confirm and fingerprint only for `dap-` ids.
    let odd = request(&row(json!({
        "id": "x-1", "tool": "t", "status": "pending",
        "source": "delegation", "node": "n", "display": "d", "fingerprint": "fp"
    })));
    assert!(!odd.is_child() && odd.child_node.is_none() && odd.description.is_none());
    let intent = |id: &str| DecisionIntent {
        id: id.into(),
        decision: Decision::AllowOnce,
        fingerprint: Some("fp".into()),
    };
    assert_eq!(decision_request(&intent("x-1")).confirm, None);
    assert!(decision_request(&intent("dap-1")).confirm.is_some());
    // A serve row's own `display` key is not a description.
    let serve = request(&row(
        json!({"id": "a", "tool": "t", "status": "pending", "display": "d"}),
    ));
    assert!(serve.description.is_none() && !serve.is_child());
}

#[test]
fn keys_of_another_type_are_not_read_as_not_reported() {
    let r = request(&row(json!({
        "id": "dap-2", "tool": "t", "status": "pending", "source": 7,
        "node": ["a"], "fingerprint": 12, "risk": {"x": 1}, "display": null
    })));
    // A risk that is there but not a string is an unknown code, a danger.
    assert_eq!(r.risk.as_deref(), Some("{\"x\":1}"));
    assert_eq!(risk_care(r.risk.as_deref().unwrap_or("")), Care::Danger);
    assert_eq!(r.fingerprint.as_deref(), Some("12"));
    assert_eq!(r.child_node.as_deref(), Some("[\"a\"]"));
    // `null` and a missing key are "not reported".
    assert!(r.description.is_none());
    let none = request(&row(
        json!({"id": "a", "tool": "t", "status": "pending", "risk": null}),
    ));
    assert!(none.risk.is_none());
    for v in [json!(3), json!(["Destructive"]), json!(true)] {
        let r = request(&row(
            json!({"id": "a", "tool": "t", "status": "pending", "risk": v}),
        ));
        assert!(r.risk.is_some(), "{v}");
    }
}

#[test]
fn a_reported_risk_is_passed_on_and_codes_are_kept() {
    let r = request(&row(json!({
        "id": "a", "tool": "t", "status": "frozen", "risk": "Destructive",
        "summary": {"kind": "teleport", "target": "x", "effect": "melts"}
    })));
    assert_eq!(r.risk.as_deref(), Some("Destructive"));
    assert_eq!(r.status, "frozen", "an unknown status is not rewritten");
    let s = r.summary.expect("summary");
    assert_eq!((s.kind.as_str(), s.effect.as_str()), ("teleport", "melts"));
}

#[test]
fn hostile_text_passes_through_untouched() {
    let evil = format!("a\u{202e}b\u{0}\u{1b}[2J\n{}", "x".repeat(200_000));
    let r = request(&row(json!({
        "id": "apr-1", "tool": evil, "status": "pending", "description": evil,
        "summary": {"kind": evil, "target": evil, "effect": evil, "preview": [evil]}
    })));
    assert_eq!(r.tool, evil);
    assert_eq!(r.description.as_deref(), Some(evil.as_str()));
    let s = r.summary.expect("summary");
    assert_eq!(
        (s.kind.as_str(), s.target.as_str()),
        (evil.as_str(), evil.as_str())
    );
    assert_eq!(s.preview.as_slice(), std::slice::from_ref(&evil));
    // The queue, not the adapter, bounds it: the card it draws is cut.
    let mut q = ApprovalsState::new();
    let list: ApprovalList = serde_json::from_value(
        json!({"approvals": [{"id": "a", "tool": evil, "status": "pending", "description": evil}]}),
    )
    .expect("list");
    let (id, start) = FetchIds::new().start(Scope::All).expect("id");
    q.apply(start);
    q.apply(snapshot(&list, Scope::All, id, 1));
    let card = q.card("a", 1).expect("card");
    assert!(card
        .details
        .iter()
        .all(|d| d.value.chars().count() <= 20_000));
    assert!(!card.subject.contains('\u{202e}'));
}

#[test]
fn a_hostile_id_reaches_the_model_to_be_refused_there() {
    let r = request(&row(
        json!({"id": "../x y", "tool": "t", "status": "pending"}),
    ));
    assert_eq!(r.id, "../x y");
    let mut q = ApprovalsState::new();
    q.apply(ApprovalsAction::Requested(r, 1));
    assert_eq!(q.refused(), 1);
}

#[test]
fn a_list_keeps_the_servers_order() {
    let list: ApprovalList = serde_json::from_value(json!({"approvals": [
        {"id": "b", "tool": "t", "status": "pending"},
        {"id": "a", "tool": "t", "status": "approved"}
    ]}))
    .expect("list");
    let ids: Vec<_> = requests(&list).into_iter().map(|r| r.id).collect();
    assert_eq!(ids, ["b", "a"]);
    assert!(requests(&ApprovalList { approvals: vec![] }).is_empty());
}

#[test]
fn a_reply_becomes_a_resolved_request_with_only_id_and_status() {
    let reply: DecisionReply =
        serde_json::from_value(json!({"id": "apr-1", "status": "approved", "resuming": true}))
            .expect("reply");
    let r = resolved_from_reply(&reply);
    assert_eq!((r.id.as_str(), r.status.as_str()), ("apr-1", "approved"));
    assert!(r.summary.is_none() && r.description.is_none());
}

#[test]
fn a_decision_maps_to_the_right_request() {
    let intent = |id: &str, d, fp: Option<&str>| DecisionIntent {
        id: id.into(),
        decision: d,
        fingerprint: fp.map(str::to_owned),
    };
    let body = |i: &DecisionIntent| serde_json::to_value(decision_request(i)).expect("json");
    assert_eq!(
        body(&intent("apr-1", Decision::AllowOnce, None)),
        json!({"decision": "allow"})
    );
    assert_eq!(
        body(&intent("apr-1", Decision::DontAllow, None)),
        json!({"decision": "deny"})
    );
    // A serve row never sends a fingerprint, even if the intent has one.
    assert_eq!(
        body(&intent("apr-1", Decision::AllowOnce, Some("fp"))),
        json!({"decision": "allow"})
    );
    assert_eq!(
        body(&intent("dap-9", Decision::AllowOnce, Some("fp-abc"))),
        json!({"decision": "allow", "confirm": "dap-9", "fingerprint": "fp-abc"})
    );
    assert_eq!(
        body(&intent("dap-9", Decision::DontAllow, Some("fp-abc"))),
        json!({"decision": "deny", "fingerprint": "fp-abc"})
    );
}

#[test]
fn the_row_for_decide_row_carries_the_fingerprint_the_client_reads() {
    let i = DecisionIntent {
        id: "dap-9".into(),
        decision: Decision::AllowOnce,
        fingerprint: Some("fp-abc".into()),
    };
    let r = row_for_decision(&i);
    assert_eq!((r.id.as_str(), r.status.as_str()), ("dap-9", "pending"));
    assert_eq!(r.extra.get("fingerprint"), Some(&json!("fp-abc")));
    let none = row_for_decision(&DecisionIntent {
        fingerprint: None,
        ..i
    });
    assert!(none.extra.is_empty());
}

#[test]
fn fetch_ids_count_from_one_and_start_names_its_id() {
    let mut ids = FetchIds::new();
    assert_eq!(ids.next_id(), Some(FetchId(1)));
    let (id, action) = ids.start(Scope::Session("ses-1".into())).expect("id");
    assert_eq!(id, FetchId(2));
    assert_eq!(
        action,
        ApprovalsAction::FetchStarted(Scope::Session("ses-1".into()), FetchId(2))
    );
    assert_eq!(
        failed(Scope::All, id),
        ApprovalsAction::FetchFailed(Scope::All, FetchId(2))
    );
    assert_eq!(scope_session(&Scope::All), None);
    assert_eq!(scope_session(&Scope::Session("s".into())), Some("s"));
}

#[test]
fn a_spent_counter_never_repeats_an_id() {
    let mut last = FetchIds::after(u64::MAX - 1);
    assert_eq!(last.next_id(), Some(FetchId(u64::MAX)));
    for _ in 0..3 {
        assert_eq!(last.next_id(), None);
        assert!(last.start(Scope::All).is_none());
    }
    assert!(follow_up(&ApprovalsAction::ConnectionLost, &mut last).is_none());
}
