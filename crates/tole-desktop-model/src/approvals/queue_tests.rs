//! Tests of the queue reducer and the composer rule.

use super::fixtures::{req, T0};
use super::*;

fn requested(state: &mut ApprovalsState, r: ApprovalRequest, now: u64) -> bool {
    state.apply(ApprovalsAction::Requested(r, now))
}

fn resolved(state: &mut ApprovalsState, id: &str, status: &str, now: u64) -> bool {
    state.apply(ApprovalsAction::Resolved(
        ApprovalRequest::new(id, "write_file", status),
        now,
    ))
}

#[test]
fn a_repeat_after_a_reconnect_changes_nothing() {
    let mut q = ApprovalsState::new();
    assert!(requested(&mut q, req("a", "s", "create_file", "adds"), T0));
    let before = q.clone();
    // Same id, later, other content: the first row stays.
    let mut again = req("a", "s", "delete_file", "cannot_be_undone");
    again.tool = "other".into();
    assert!(!requested(&mut q, again, T0 + 5_000));
    assert_eq!(
        (q == before, q.waiting_count(), q.selected()),
        (true, 1, Some("a"))
    );
}

#[test]
fn resolved_wins_whatever_the_arrival_order() {
    let mut q = ApprovalsState::new();
    requested(&mut q, req("a", "s", "create_file", "adds"), T0);
    resolved(&mut q, "a", "approved", T0 + 1);
    requested(&mut q, req("a", "s", "create_file", "adds"), T0 + 2);
    assert_eq!(
        q.waiting_count(),
        0,
        "a late `requested` does not bring it back"
    );
    // `resolved` of a request never seen: decided, with its row.
    let mut early = req("b", "s", "create_file", "adds");
    early.status = "denied".into();
    q.apply(ApprovalsAction::Resolved(early, T0 + 3));
    requested(&mut q, req("b", "s", "create_file", "adds"), T0 + 4);
    let labels: Vec<_> = q
        .rows(Tab::Decided, T0 + 5)
        .iter()
        .map(|r| r.outcome_label)
        .collect();
    assert_eq!(labels, [Some("Not allowed"), Some("Allowed")]);
    // The first answer is final; a frame that says "pending" is no decision.
    resolved(&mut q, "a", "denied", T0 + 6);
    assert_eq!(
        q.rows(Tab::Decided, T0 + 7)[1].outcome,
        Some(Outcome::Allowed)
    );
    assert!(!resolved(&mut q, "c", "pending", T0 + 8));
}

#[test]
fn the_wait_runs_out_into_expired_without_a_click() {
    let mut q = ApprovalsState::new();
    requested(&mut q, req("a", "s", "run_program", "runs_program"), T0);
    assert!(!q.apply(ApprovalsAction::Tick(T0 + 899_000)));
    assert_eq!(
        q.rows(Tab::Waiting, T0 + 899_000)[0].left.as_deref(),
        Some("1 s left")
    );
    assert!(q.apply(ApprovalsAction::Tick(T0 + 900_000)));
    let row = &q.rows(Tab::Decided, T0 + 900_000)[0];
    assert_eq!((q.waiting_count(), row.outcome_label), (0, Some("Expired")));
    let c = q.card("a", T0 + 900_000).unwrap();
    assert!(!c.answerable() && c.state_note.is_some());
    // A click after the time is up allows nothing, whether or not a tick ran.
    assert_eq!(
        q.answer("a", Decision::AllowOnce, T0 + 900_000),
        Err(AnswerError::NotWaiting)
    );
    requested(&mut q, req("e", "s", "create_file", "adds"), T0);
    let late = T0 + 900_000;
    assert_eq!(
        q.answer("e", Decision::AllowOnce, late),
        Err(AnswerError::Expired)
    );
    assert_eq!(
        q.answer("zz", Decision::DontAllow, late),
        Err(AnswerError::Unknown)
    );
    // The server's word replaces the local guess (it was answered in time).
    resolved(&mut q, "a", "approved", T0 + 901_000);
    assert_eq!(
        q.rows(Tab::Decided, T0 + 902_000)[0].outcome,
        Some(Outcome::Allowed)
    );
}

#[test]
fn an_answer_is_sent_once_and_the_first_reply_wins() {
    let mut q = ApprovalsState::new();
    let mut r = req("dap-7", "s", "create_file", "adds");
    r.fingerprint = Some("fp".into());
    requested(&mut q, r, T0);
    let intent = q.answer("dap-7", Decision::AllowOnce, T0 + 10).unwrap();
    assert_eq!(
        (intent.fingerprint.as_deref(), intent.decision),
        (Some("fp"), Decision::AllowOnce)
    );
    // No second answer while the first is on its way, and no buttons.
    assert_eq!(
        q.answer("dap-7", Decision::DontAllow, T0 + 11),
        Err(AnswerError::NotWaiting)
    );
    assert!(!q.card("dap-7", T0 + 11).unwrap().answerable());
    // A failed call gives the request back.
    q.apply(ApprovalsAction::AnswerFailed(
        "dap-7".into(),
        false,
        T0 + 12,
    ));
    assert!(q.card("dap-7", T0 + 12).unwrap().state == CardState::Waiting);
    // A conflict (someone answered first): no buttons, the list is stale.
    q.answer("dap-7", Decision::DontAllow, T0 + 13).unwrap();
    q.apply(ApprovalsAction::AnswerFailed("dap-7".into(), true, T0 + 14));
    let card = q.card("dap-7", T0 + 15).unwrap();
    assert!(q.is_stale() && !card.answerable());
    assert_eq!(card.state, CardState::Decided(Outcome::Closed));
    q.apply(ApprovalsAction::FetchStarted(Scope::All, FetchId(1)));
    q.apply(ApprovalsAction::Snapshot(
        vec![],
        Scope::All,
        FetchId(1),
        T0 + 902_000,
    ));
    assert!(!q.is_stale());
}

#[test]
fn a_snapshot_decides_what_the_server_no_longer_lists() {
    let mut q = ApprovalsState::new();
    requested(&mut q, req("a", "s1", "create_file", "adds"), T0);
    requested(&mut q, req("b", "s2", "create_file", "adds"), T0);
    let mut c = req("c", "s2", "create_file", "adds");
    c.status = "expired".into();
    // A snapshot of one session only touches that session.
    q.apply(ApprovalsAction::FetchStarted(
        Scope::Session("s1".into()),
        FetchId(1),
    ));
    q.apply(ApprovalsAction::Snapshot(
        vec![c],
        Scope::Session("s1".into()),
        FetchId(1),
        T0 + 1,
    ));
    let decided = q.rows(Tab::Decided, T0 + 2);
    let outcome = |id: &str| decided.iter().find(|r| r.id == id).map(|r| r.outcome);
    assert_eq!(
        (q.waiting_count(), outcome("a")),
        (1, Some(Some(Outcome::Closed)))
    );
    assert_eq!(outcome("c"), Some(Some(Outcome::Expired)));
    // "No longer waiting" is never worded as allowed; the server's word wins later.
    resolved(&mut q, "a", "approved", T0 + 3);
    assert_eq!(
        q.rows(Tab::Decided, T0 + 4)
            .iter()
            .find(|r| r.id == "a")
            .unwrap()
            .outcome,
        Some(Outcome::Allowed)
    );
}

#[test]
fn the_composer_is_disabled_while_the_turn_waits() {
    let mut q = ApprovalsState::new();
    q.apply(ApprovalsAction::FetchStarted(Scope::All, FetchId(1)));
    q.apply(ApprovalsAction::Snapshot(
        vec![],
        Scope::All,
        FetchId(1),
        T0 - 1,
    ));
    assert_eq!(q.composer("s1"), ComposerState::Open);
    requested(&mut q, req("a", "s1", "create_file", "adds"), T0);
    requested(&mut q, req("dap-1", "", "create_file", "adds"), T0);
    assert_eq!(
        q.composer("s1"),
        ComposerState::WaitingForAnswer { requests: 1 }
    );
    assert!(!q.composer("s2").is_disabled());
    assert_eq!(
        q.composer("s1").hint(),
        Some("Answer the request above to continue...")
    );
    // An answer on its way still counts; the decision opens the box.
    q.answer("a", Decision::DontAllow, T0 + 1).unwrap();
    assert!(q.composer("s1").is_disabled());
    resolved(&mut q, "a", "denied", T0 + 2);
    assert_eq!(
        (q.composer("s1"), q.composer("s1").hint()),
        (ComposerState::Open, None)
    );
}

#[test]
fn lists_are_ordered_and_the_decided_list_is_bounded() {
    let mut q = ApprovalsState::new();
    let mut late = req("late", "s", "create_file", "adds");
    late.created_at_ms = Some(T0 + 60_000);
    requested(&mut q, late, T0 + 60_000);
    requested(
        &mut q,
        req("early", "s", "create_file", "adds"),
        T0 + 61_000,
    );
    let ids: Vec<String> = q
        .rows(Tab::Waiting, T0 + 62_000)
        .into_iter()
        .map(|r| r.id)
        .collect();
    assert_eq!(ids, ["early", "late"]);
    for i in 0..MAX_DECIDED + 5 {
        resolved(&mut q, &format!("d{i}"), "denied", T0 + 100_000 + i as u64);
    }
    let decided = q.rows(Tab::Decided, T0 + 400_000);
    assert_eq!((decided.len(), q.waiting_count()), (MAX_DECIDED, 2));
    assert_eq!(decided[0].id, format!("d{}", MAX_DECIDED + 4));
}
