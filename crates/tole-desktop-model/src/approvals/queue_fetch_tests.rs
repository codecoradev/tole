//! Round-three review of the queue (#1032): fetch identity, unreadable
//! statuses, decided order, the server's pending word, selection.

use super::fixtures::{req, T0};
use super::*;
use crate::skills::Badge;

fn requested(q: &mut ApprovalsState, r: ApprovalRequest, now: u64) -> bool {
    q.apply(ApprovalsAction::Requested(r, now))
}

fn resolved(q: &mut ApprovalsState, id: &str, status: &str, now: u64) -> bool {
    let r = ApprovalRequest::new(id, "write_file", status);
    q.apply(ApprovalsAction::Resolved(r, now))
}

fn started(q: &mut ApprovalsState, scope: Scope, id: u64) {
    q.apply(ApprovalsAction::FetchStarted(scope, FetchId(id)));
}

fn reply(q: &mut ApprovalsState, rows: Vec<ApprovalRequest>, scope: Scope, id: u64, now: u64) {
    q.apply(ApprovalsAction::Snapshot(rows, scope, FetchId(id), now));
}

fn known() -> ApprovalsState {
    let mut q = ApprovalsState::new();
    started(&mut q, Scope::All, 1);
    reply(&mut q, vec![], Scope::All, 1, T0 - 1);
    q
}

fn pending(id: &str, session: &str) -> ApprovalRequest {
    req(id, session, "create_file", "adds")
}

fn history(n: u64, created: u64) -> Vec<ApprovalRequest> {
    (0..n)
        .map(|i| {
            let mut r = pending(&format!("h{i}"), "s");
            r.status = "denied".into();
            r.created_at_ms = Some(created + i);
            r
        })
        .collect()
}

fn outcome(q: &ApprovalsState, id: &str) -> Option<Outcome> {
    let rows = q.rows(Tab::Decided, T0);
    rows.iter().find(|r| r.id == id).and_then(|r| r.outcome)
}

#[test]
fn a_second_fetch_does_not_let_the_first_reply_close_what_it_cannot_know() {
    let mut q = known();
    started(&mut q, Scope::All, 2);
    requested(&mut q, pending("r", "s"), T0);
    started(&mut q, Scope::All, 3);
    // The reply of fetch 2, computed before `r` existed, is not the current one.
    reply(&mut q, vec![], Scope::All, 2, T0 + 1);
    assert_eq!(q.waiting_badge(), Badge::Count(1));
    assert_eq!(
        q.composer("s"),
        ComposerState::WaitingForAnswer { requests: 1 }
    );
    // The current one counts, and still keeps what the older mark cannot justify.
    reply(&mut q, vec![], Scope::All, 3, T0 + 2);
    assert_eq!(q.waiting_count(), 1);
}

#[test]
fn a_reply_that_is_not_the_current_fetch_never_restores_what_is_known() {
    // Started before a loss, answered after the next fetch started.
    let mut q = known();
    started(&mut q, Scope::All, 2);
    q.apply(ApprovalsAction::ConnectionLost);
    started(&mut q, Scope::All, 3);
    reply(&mut q, vec![pending("a", "s")], Scope::All, 2, T0);
    assert_eq!(q.waiting_badge(), Badge::Unknown);
    assert_eq!(q.composer("other"), ComposerState::Checking);
    // Its rows are merged only when no fetch runs: the current one could not
    // close what the stale reply creates now (#1116).
    assert_eq!(q.waiting_count(), 0);
    reply(&mut q, vec![pending("a", "s")], Scope::All, 3, T0 + 1);
    assert_eq!(q.waiting_badge(), Badge::Count(1));
    // The failure of a superseded fetch says nothing about the current one.
    started(&mut q, Scope::All, 4);
    started(&mut q, Scope::All, 5);
    assert!(!q.apply(ApprovalsAction::FetchFailed(Scope::All, FetchId(4))));
    assert_eq!(q.waiting_badge(), Badge::Count(1));
    assert!(q.apply(ApprovalsAction::FetchFailed(Scope::All, FetchId(5))));
    assert_eq!(q.waiting_badge(), Badge::Unknown);
    // A reply that never started a fetch is merge only.
    reply(&mut q, vec![], Scope::All, 9, T0 + 2);
    assert_eq!(q.waiting_badge(), Badge::Unknown);
}

#[test]
fn a_stale_list_asks_for_a_fetch_only_while_none_runs_and_only_all_settles_it() {
    let mut q = known();
    requested(&mut q, pending("a", "s"), T0);
    q.answer("a", Decision::AllowOnce, T0 + 1).unwrap();
    q.apply(ApprovalsAction::AnswerFailed("a".into(), true, T0 + 2));
    assert!(q.is_stale());
    started(&mut q, Scope::Session("other".into()), 2);
    reply(&mut q, vec![], Scope::Session("other".into()), 2, T0 + 3);
    assert!(q.is_stale(), "another session's list says nothing about it");
    started(&mut q, Scope::All, 3);
    assert!(!q.is_stale(), "a fetch is running");
    reply(&mut q, vec![], Scope::All, 3, T0 + 4);
    assert!(!q.is_stale());
}

#[test]
fn a_status_this_version_cannot_read_is_possibly_pending() {
    let mut q = known();
    let mut r = pending("b", "s");
    r.status = "awaiting".into();
    requested(&mut q, r, T0);
    assert_eq!(q.waiting_count(), 0);
    assert_eq!(q.waiting_badge(), Badge::Unknown);
    assert_eq!(q.composer("s"), ComposerState::Checking);
    assert_eq!(q.composer("other"), ComposerState::Open);
    // A known status settles it.
    resolved(&mut q, "b", "denied", T0 + 1);
    assert_eq!(q.waiting_badge(), Badge::Count(0));
    assert_eq!(q.composer("s"), ComposerState::Open);
    // So does the end of its wait.
    let mut r = pending("c", "s");
    r.status = "awaiting".into();
    requested(&mut q, r, T0);
    assert_eq!(q.waiting_badge(), Badge::Unknown);
    q.apply(ApprovalsAction::Tick(T0 + 900_000));
    assert_eq!(q.waiting_badge(), Badge::Count(0));
}

#[test]
fn history_is_ordered_by_decision_not_by_the_clock() {
    // A clock that steps back does not evict the answer just given.
    let mut q = known();
    for i in 0..MAX_DECIDED as u64 {
        resolved(&mut q, &format!("d{i}"), "denied", T0 + 10_000_000 + i);
    }
    resolved(&mut q, "mine", "approved", T0 + 1);
    assert_eq!(outcome(&q, "mine"), Some(Outcome::Allowed));
    assert_eq!(outcome(&q, "d0"), None, "the oldest decision went instead");
    assert_eq!(q.rows(Tab::Decided, T0)[0].id, "mine");
    // A request decided by a snapshot is shown, not trimmed at once.
    let mut q = known();
    requested(&mut q, pending("w", "s"), T0);
    started(&mut q, Scope::All, 2);
    let mut rows = history(MAX_DECIDED as u64, T0 + 10);
    let mut w = pending("w", "s");
    w.status = "approved".into();
    rows.push(w);
    reply(&mut q, rows, Scope::All, 2, T0 + 1_000);
    assert_eq!(outcome(&q, "w"), Some(Outcome::Allowed));
    assert_eq!(q.rows(Tab::Decided, T0).len(), MAX_DECIDED);
}

#[test]
fn the_servers_word_that_corrects_a_guess_is_not_cut_away() {
    let mut q = known();
    requested(&mut q, pending("w", "s"), T0);
    q.apply(ApprovalsAction::Tick(T0 + 900_000));
    assert_eq!(outcome(&q, "w"), Some(Outcome::Expired));
    started(&mut q, Scope::All, 2);
    let mut rows = history(250, T0 + 900_010);
    let mut w = pending("w", "s");
    w.status = "approved".into();
    rows.push(w);
    reply(&mut q, rows, Scope::All, 2, T0 + 901_000);
    assert_eq!(outcome(&q, "w"), Some(Outcome::Allowed));
}

#[test]
fn a_snapshot_that_lists_a_request_pending_is_believed_over_the_local_clock() {
    // Local clock 40 s ahead of the server's.
    let mut q = known();
    let mut w = pending("w", "s");
    w.created_at_ms = Some(T0 - 40_000);
    requested(&mut q, w.clone(), T0);
    let late = T0 + 860_001;
    q.apply(ApprovalsAction::Tick(late));
    assert_eq!(outcome(&q, "w"), Some(Outcome::Expired));
    assert!(
        !requested(&mut q, w.clone(), late),
        "a frame is not believed"
    );
    started(&mut q, Scope::All, 2);
    reply(&mut q, vec![w.clone()], Scope::All, 2, late + 1);
    assert_eq!(q.waiting_badge(), Badge::Count(1));
    assert_eq!(
        q.composer("s"),
        ComposerState::WaitingForAnswer { requests: 1 }
    );
    // The grace is the skew, no more.
    assert!(!q.apply(ApprovalsAction::Tick(late + 50_000)));
    assert!(q.apply(ApprovalsAction::Tick(late + 70_000)));
    assert_eq!(q.waiting_badge(), Badge::Count(0));
    // An id trimmed as a guess is shown again; one the server decided is not.
    let mut q = known();
    requested(&mut q, pending("g", "s"), T0);
    q.apply(ApprovalsAction::Tick(T0 + 900_000));
    resolved(&mut q, "final", "denied", T0 + 900_001);
    for i in 0..MAX_DECIDED as u64 {
        resolved(&mut q, &format!("d{i}"), "denied", T0 + 900_002 + i);
    }
    assert_eq!((outcome(&q, "g"), outcome(&q, "final")), (None, None));
    assert!(!requested(&mut q, pending("g", "s"), T0 + 1_000_000));
    started(&mut q, Scope::All, 2);
    let rows = vec![pending("g", "s"), pending("final", "s")];
    reply(&mut q, rows, Scope::All, 2, T0 + 1_000_001);
    let ids: Vec<_> = q
        .rows(Tab::Waiting, T0 + 1_000_002)
        .into_iter()
        .map(|r| r.id)
        .collect();
    assert_eq!(ids, ["g"]);
}

#[test]
fn a_snapshot_that_closes_the_selected_card_does_not_select_another() {
    let mut q = known();
    requested(&mut q, pending("a", "s"), T0);
    assert_eq!(q.selected(), Some("a"));
    started(&mut q, Scope::All, 2);
    reply(&mut q, vec![pending("b", "s")], Scope::All, 2, T0 + 1);
    assert_eq!(q.waiting_count(), 1);
    assert_eq!((q.selected(), q.selected_card(T0).is_none()), (None, true));
    // With nothing selected before it, the snapshot selects.
    let mut q = known();
    started(&mut q, Scope::All, 2);
    reply(&mut q, vec![pending("b", "s")], Scope::All, 2, T0 + 1);
    assert_eq!(q.selected(), Some("b"));
}

#[test]
fn a_request_promoted_from_the_overflow_keeps_the_wait_it_had() {
    let mut q = known();
    for i in 0..MAX_WAITING {
        requested(&mut q, pending(&format!("w{i}"), "s"), T0);
    }
    let mut child = ApprovalRequest::new("dap-1", "t", "pending");
    child.expires_in_secs = Some(120);
    requested(&mut q, child.clone(), T0);
    assert_eq!(q.waiting_at_least(), MAX_WAITING + 1);
    resolved(&mut q, "w0", "denied", T0 + 1);
    let now = T0 + 100_000;
    assert!(requested(&mut q, child, now));
    let row = q
        .rows(Tab::Waiting, now)
        .into_iter()
        .find(|r| r.id == "dap-1");
    assert_eq!(row.unwrap().left.as_deref(), Some("20 s left"));
}
