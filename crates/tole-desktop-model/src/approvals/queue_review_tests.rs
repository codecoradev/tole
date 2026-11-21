//! Round-two review of the queue (#1032): what a snapshot may restore, what a
//! burst leaves behind, refusals, decided rows over the cap, the cheap row,
//! the refresh when room frees.

use super::card::headline;
use super::fixtures::{req, T0};
use super::*;
use crate::skills::Badge;

fn requested(q: &mut ApprovalsState, r: ApprovalRequest, now: u64) -> bool {
    q.apply(ApprovalsAction::Requested(r, now))
}

fn started(q: &mut ApprovalsState, scope: Scope) {
    q.apply(ApprovalsAction::FetchStarted(scope, FetchId(1)));
}

fn snapshot(q: &mut ApprovalsState, rows: Vec<ApprovalRequest>, scope: Scope, now: u64) {
    q.apply(ApprovalsAction::Snapshot(rows, scope, FetchId(1), now));
}

fn known() -> ApprovalsState {
    let mut q = ApprovalsState::new();
    started(&mut q, Scope::All);
    snapshot(&mut q, vec![], Scope::All, T0 - 1);
    q
}

fn pending(id: &str, session: &str) -> ApprovalRequest {
    req(id, session, "create_file", "adds")
}

fn bad(session: &str) -> ApprovalRequest {
    pending("bad id", session)
}

#[test]
fn a_burst_over_the_cap_does_not_leave_the_list_unknown_for_good() {
    let mut q = known();
    for i in 0..1_300 {
        requested(&mut q, pending(&format!("w{i}"), "s"), T0);
    }
    assert_eq!(q.waiting_badge(), Badge::Unknown);
    // They all run out; nothing is listed, the count is still not trusted.
    q.apply(ApprovalsAction::Tick(T0 + 900_000));
    assert_eq!(q.waiting_at_least(), 0);
    assert_eq!(q.waiting_badge(), Badge::Unknown);
    assert!(q.is_stale(), "room freed: the adapter is asked to refresh");
    // A full list asked for after the burst restores what is known.
    started(&mut q, Scope::All);
    snapshot(&mut q, vec![], Scope::All, T0 + 900_001);
    assert_eq!(q.waiting_badge(), Badge::Count(0));
    assert_eq!(q.composer("s"), ComposerState::Open);
    assert!(!q.is_stale());
}

#[test]
fn a_late_reply_after_a_loss_does_not_make_the_list_known() {
    for loss in [
        ApprovalsAction::ConnectionLost,
        ApprovalsAction::FetchFailed(Scope::All, FetchId(1)),
    ] {
        let mut q = known();
        started(&mut q, Scope::All);
        q.apply(loss);
        // The reply of the fetch that was in flight arrives anyway.
        snapshot(&mut q, vec![pending("a", "s")], Scope::All, T0);
        assert_eq!(q.waiting_badge(), Badge::Unknown);
        assert_eq!(q.composer("other"), ComposerState::Checking);
        // Its rows are merged, though.
        assert_eq!(
            q.composer("s"),
            ComposerState::WaitingForAnswer { requests: 1 }
        );
        // A fetch asked for after the loss does restore it.
        started(&mut q, Scope::All);
        snapshot(&mut q, vec![pending("a", "s")], Scope::All, T0 + 1);
        assert_eq!(q.waiting_badge(), Badge::Count(1));
        assert_eq!(q.composer("other"), ComposerState::Open);
    }
}

#[test]
fn only_a_snapshot_that_answers_a_fetch_makes_the_list_known() {
    let mut q = ApprovalsState::new();
    // No `FetchStarted` at all: rows are merged, nothing is known or closed.
    snapshot(&mut q, vec![pending("a", "s")], Scope::All, T0);
    assert_eq!((q.waiting_count(), q.waiting_badge()), (1, Badge::Unknown));
    snapshot(&mut q, vec![], Scope::All, T0 + 1);
    assert_eq!(q.waiting_count(), 1);
    assert_eq!(q.composer("x"), ComposerState::Checking);
    // The normal path.
    started(&mut q, Scope::All);
    snapshot(&mut q, vec![pending("a", "s")], Scope::All, T0 + 2);
    assert_eq!(q.waiting_badge(), Badge::Count(1));
    // One reply answers one fetch: a second reply is unmarked.
    q.apply(ApprovalsAction::ConnectionLost);
    snapshot(&mut q, vec![], Scope::All, T0 + 3);
    assert_eq!(q.waiting_badge(), Badge::Unknown);
}

#[test]
fn a_session_reply_after_a_loss_does_not_open_the_box() {
    let mut q = ApprovalsState::new();
    started(&mut q, Scope::Session("s".into()));
    q.apply(ApprovalsAction::ConnectionLost);
    snapshot(&mut q, vec![], Scope::Session("s".into()), T0);
    assert_eq!(q.composer("s"), ComposerState::Checking);
}

#[test]
fn a_refused_pending_row_keeps_the_task_it_names_checking() {
    let mut q = known();
    requested(&mut q, bad("s1"), T0);
    assert_eq!(q.waiting_badge(), Badge::Unknown);
    assert_eq!(q.composer("s1"), ComposerState::Checking);
    assert_eq!(q.composer("s2"), ComposerState::Open);
    // A session id that is not usable could be any task's.
    requested(&mut q, bad("no good!"), T0);
    assert_eq!(q.composer("s2"), ComposerState::Checking);
    // A row with no task names none.
    let mut q = known();
    let mut child = pending("bad id", "");
    child.session_id = None;
    requested(&mut q, child, T0);
    assert_eq!(q.composer("s2"), ComposerState::Open);
    assert_eq!(q.waiting_badge(), Badge::Unknown);
}

#[test]
fn a_snapshot_only_accounts_for_refusals_older_than_its_fetch() {
    let mut q = known();
    requested(&mut q, bad("s1"), T0); // before the fetch: the list has it
    started(&mut q, Scope::All);
    requested(&mut q, bad("s2"), T0 + 1); // after: the list cannot have it
    snapshot(&mut q, vec![], Scope::All, T0 + 2);
    assert_eq!(q.refused(), 1);
    assert_eq!(q.waiting_badge(), Badge::Unknown);
    assert_eq!(q.composer("s2"), ComposerState::Checking);
    assert_eq!(q.composer("s1"), ComposerState::Open);
    // The next full list after it settles it.
    started(&mut q, Scope::All);
    snapshot(&mut q, vec![], Scope::All, T0 + 3);
    assert_eq!((q.refused(), q.waiting_badge()), (0, Badge::Count(0)));
}

#[test]
fn a_request_the_list_reports_decided_never_stays_answerable_past_the_cap() {
    let mut q = known();
    requested(&mut q, pending("w", "s"), T0);
    started(&mut q, Scope::All);
    let mut rows: Vec<_> = (0..MAX_DECIDED as u64 + 5)
        .map(|i| {
            let mut r = pending(&format!("h{i}"), "s");
            r.status = "denied".into();
            r.created_at_ms = Some(T0 + 10 + i);
            r
        })
        .collect();
    let mut w = pending("w", "s");
    w.status = "approved".into();
    rows.push(w);
    snapshot(&mut q, rows, Scope::All, T0 + 1_000);
    assert_eq!(q.waiting_count(), 0);
    assert_eq!(q.composer("s"), ComposerState::Open);
    assert_eq!(
        q.answer("w", Decision::AllowOnce, T0 + 1_001),
        Err(AnswerError::NotWaiting)
    );
    // And it is not forgotten with the old history: it shows its outcome.
    let rows = q.rows(Tab::Decided, T0 + 1_001);
    assert!(rows
        .iter()
        .any(|r| r.id == "w" && r.outcome == Some(Outcome::Allowed)));
}

#[test]
fn room_that_frees_asks_for_a_refresh_and_the_refresh_lists_the_overflow() {
    let mut q = known();
    for i in 0..=MAX_WAITING {
        requested(&mut q, pending(&format!("w{i}"), "s"), T0);
    }
    assert_eq!(
        (q.waiting_count(), q.waiting_at_least()),
        (MAX_WAITING, MAX_WAITING + 1)
    );
    assert!(!q.is_stale());
    q.answer("w0", Decision::DontAllow, T0 + 1).unwrap();
    q.apply(ApprovalsAction::Resolved(
        ApprovalRequest::new("w0", "write_file", "denied"),
        T0 + 2,
    ));
    assert!(q.is_stale(), "an overflowed request can now be listed");
    started(&mut q, Scope::All);
    let rows = (1..=MAX_WAITING)
        .map(|i| pending(&format!("w{i}"), "s"))
        .collect();
    snapshot(&mut q, rows, Scope::All, T0 + 3);
    assert_eq!(q.waiting_count(), MAX_WAITING);
    assert_eq!(q.waiting_badge(), Badge::Count(MAX_WAITING));
    assert!(!q.is_stale());
}

#[test]
fn a_conflict_leaves_no_buttons_and_a_pending_row_can_revive_it() {
    let mut q = known();
    requested(&mut q, pending("a", "s"), T0);
    q.answer("a", Decision::AllowOnce, T0 + 1).unwrap();
    q.apply(ApprovalsAction::AnswerFailed("a".into(), true, T0 + 2));
    assert!(!q.card("a", T0 + 3).unwrap().answerable());
    assert_eq!(q.composer("s"), ComposerState::Open);
    started(&mut q, Scope::All);
    snapshot(&mut q, vec![pending("a", "s")], Scope::All, T0 + 4);
    assert!(q.card("a", T0 + 5).unwrap().answerable());
}

#[test]
fn a_card_always_agrees_with_the_phase_of_its_entry() {
    let mut q = known();
    // A status this version does not know, then the server's word.
    let mut r = pending("a", "s");
    r.status = "Pending".into();
    requested(&mut q, r, T0);
    assert_eq!(
        q.card("a", T0).unwrap().state,
        CardState::Decided(Outcome::Closed)
    );
    // A pending row revives it, and the card is a waiting card again.
    requested(&mut q, pending("a", "s"), T0 + 1);
    assert_eq!(q.waiting_count(), 1);
    assert_eq!(q.card("a", T0 + 1).unwrap().state, CardState::Waiting);
    // A guess replaced by the server's word reads as that word.
    let mut r = pending("b", "s");
    r.status = "???".into();
    q.apply(ApprovalsAction::Resolved(r, T0));
    q.apply(ApprovalsAction::Resolved(
        ApprovalRequest::new("b", "write_file", "approved"),
        T0 + 2,
    ));
    assert_eq!(
        q.card("b", T0 + 3).unwrap().state,
        CardState::Decided(Outcome::Allowed)
    );
}

#[test]
fn a_row_says_what_the_card_says() {
    let mut q = known();
    let mut outside = pending("o", "s");
    if let Some(s) = outside.summary.as_mut() {
        s.inside_workspace = false;
        s.target = "../etc/hosts".into();
    }
    let mut child = ApprovalRequest::new("dap-1", "frobnicate", "pending");
    child.risk = Some("Destructive".into());
    for r in [
        outside,
        child,
        req("c", "s", "delete_file", "cannot_be_undone"),
        req("m", "s", "run_program", "adds"),
    ] {
        requested(&mut q, r, T0);
    }
    for row in q.rows(Tab::Waiting, T0 + 5_000) {
        let card = q.card(&row.id, T0 + 5_000).unwrap();
        assert_eq!(
            (
                row.wording,
                row.care,
                row.outside_folder,
                row.subject.as_str()
            ),
            (
                card.wording,
                card.care,
                card.outside_folder,
                card.subject.as_str()
            ),
            "{}",
            row.id
        );
        assert_eq!(row.left, card.left_short);
    }
    let r = ApprovalRequest::new("x", "t", "pending");
    let h = headline(&r, &r.summary_or_unknown());
    assert!(!h.outside_folder && !h.consistent);
}
