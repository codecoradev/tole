//! Tests of what the queue knows, what it bounds and how it settles
//! (the review findings of #1032): Unknown, snapshot races, lost answers,
//! tombstones, caps, hostile ids, selection.

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

fn snapshot(q: &mut ApprovalsState, rows: Vec<ApprovalRequest>, scope: Scope, now: u64) {
    q.apply(ApprovalsAction::Snapshot(rows, scope, FetchId(1), now));
}

/// A state that has loaded the whole list once.
fn known() -> ApprovalsState {
    let mut q = ApprovalsState::new();
    q.apply(ApprovalsAction::FetchStarted(Scope::All, FetchId(1)));
    snapshot(&mut q, vec![], Scope::All, T0 - 1);
    q
}

fn pending(id: &str, session: &str) -> ApprovalRequest {
    req(id, session, "create_file", "adds")
}

fn outcome(q: &ApprovalsState, id: &str, now: u64) -> Option<Outcome> {
    let rows = q.rows(Tab::Decided, now);
    rows.iter().find(|r| r.id == id).and_then(|r| r.outcome)
}

#[test]
fn an_unloaded_list_is_unknown_not_zero_and_the_box_does_not_claim_nothing_waits() {
    let mut q = ApprovalsState::new();
    assert_eq!(q.waiting_badge(), Badge::Unknown);
    let c = q.composer("s1");
    assert_eq!(c, ComposerState::Checking);
    assert!(c.is_disabled() && c.hint() == Some("Checking whether Tole is waiting for you..."));
    // A frame alone does not make the list known.
    requested(&mut q, pending("a", "s2"), T0);
    assert_eq!(
        (q.waiting_badge(), q.waiting_at_least()),
        (Badge::Unknown, 1)
    );
    // A successful fetch does.
    q.apply(ApprovalsAction::FetchStarted(Scope::All, FetchId(1)));
    snapshot(&mut q, vec![pending("a", "s2")], Scope::All, T0 + 1);
    assert_eq!(q.waiting_badge(), Badge::Count(1));
    assert_eq!(q.composer("s1"), ComposerState::Open);
    assert_eq!(
        q.composer("s2"),
        ComposerState::WaitingForAnswer { requests: 1 }
    );
}

#[test]
fn a_lost_connection_or_a_failed_fetch_makes_the_list_unknown_until_a_fetch_succeeds() {
    for lost in [
        ApprovalsAction::ConnectionLost,
        ApprovalsAction::FetchFailed(Scope::All, FetchId(1)),
    ] {
        let mut q = known();
        assert_eq!(q.waiting_badge(), Badge::Count(0));
        started(&mut q, Scope::All, 7);
        let lost = match lost {
            ApprovalsAction::FetchFailed(s, _) => ApprovalsAction::FetchFailed(s, FetchId(7)),
            other => other,
        };
        assert!(q.apply(lost));
        assert_eq!(q.waiting_badge(), Badge::Unknown);
        assert_eq!(q.composer("s1"), ComposerState::Checking);
        q.apply(ApprovalsAction::FetchStarted(Scope::All, FetchId(1)));
        snapshot(&mut q, vec![], Scope::All, T0);
        assert_eq!(q.waiting_badge(), Badge::Count(0));
        assert_eq!(q.composer("s1"), ComposerState::Open, "it recovers");
    }
}

#[test]
fn the_list_of_one_task_opens_only_that_tasks_box() {
    let mut q = ApprovalsState::new();
    q.apply(ApprovalsAction::FetchStarted(
        Scope::Session("s1".into()),
        FetchId(1),
    ));
    snapshot(&mut q, vec![], Scope::Session("s1".into()), T0);
    assert_eq!(q.composer("s1"), ComposerState::Open);
    assert_eq!(q.composer("s2"), ComposerState::Checking);
    assert_eq!(q.waiting_badge(), Badge::Unknown);
    q.apply(ApprovalsAction::FetchStarted(
        Scope::Session("s1".into()),
        FetchId(2),
    ));
    assert!(q.apply(ApprovalsAction::FetchFailed(
        Scope::Session("s1".into()),
        FetchId(2)
    )));
    assert_eq!(q.composer("s1"), ComposerState::Checking);
}

#[test]
fn a_snapshot_does_not_close_what_the_stream_announced_after_its_fetch_started() {
    let mut q = known();
    requested(&mut q, pending("old", "s"), T0);
    q.apply(ApprovalsAction::FetchStarted(Scope::All, FetchId(1)));
    requested(&mut q, pending("fresh", "s"), T0 + 1);
    snapshot(&mut q, vec![], Scope::All, T0 + 2);
    assert_eq!(outcome(&q, "old", T0 + 3), Some(Outcome::Closed));
    assert_eq!(q.waiting_count(), 1, "the newer request stays");
    // Without a `FetchStarted` a snapshot closes nothing.
    snapshot(&mut q, vec![], Scope::All, T0 + 4);
    assert_eq!(q.waiting_count(), 1);
}

#[test]
fn a_request_closed_by_omission_comes_back_when_the_server_says_pending() {
    let mut q = known();
    requested(&mut q, pending("a", "s"), T0);
    q.apply(ApprovalsAction::FetchStarted(Scope::All, FetchId(1)));
    snapshot(&mut q, vec![], Scope::All, T0 + 1);
    assert_eq!(q.waiting_count(), 0);
    // The repeat frame after a reconnect.
    assert!(requested(&mut q, pending("a", "s"), T0 + 2));
    assert_eq!((q.waiting_count(), q.selected()), (1, Some("a")));
    // A later snapshot that lists it pending, too.
    q.apply(ApprovalsAction::FetchStarted(Scope::All, FetchId(1)));
    snapshot(&mut q, vec![], Scope::All, T0 + 3);
    snapshot(&mut q, vec![pending("a", "s")], Scope::All, T0 + 4);
    assert_eq!(q.waiting_count(), 1);
    // The server's own word is final: a pending frame does not undo it.
    resolved(&mut q, "a", "closed", T0 + 5);
    requested(&mut q, pending("a", "s"), T0 + 6);
    assert_eq!(q.waiting_count(), 0);
}

#[test]
fn an_expiry_by_the_local_clock_is_not_revived_after_its_time() {
    let mut q = known();
    requested(&mut q, pending("a", "s"), T0);
    q.apply(ApprovalsAction::Tick(T0 + 900_000));
    assert!(!requested(&mut q, pending("a", "s"), T0 + 900_001));
    assert_eq!(outcome(&q, "a", T0 + 900_002), Some(Outcome::Expired));
}

#[test]
fn an_answer_that_never_gets_a_reply_does_not_block_the_task_past_the_deadline() {
    let mut q = known();
    requested(&mut q, pending("a", "s"), T0);
    q.answer("a", Decision::AllowOnce, T0 + 1).unwrap();
    assert!(q.composer("s").is_disabled());
    assert!(!q.apply(ApprovalsAction::Tick(T0 + 899_999)));
    assert!(q.apply(ApprovalsAction::Tick(T0 + 900_000)));
    assert_eq!(
        (q.waiting_count(), q.composer("s")),
        (0, ComposerState::Open)
    );
    let card = q.card("a", T0 + 900_001).unwrap();
    assert!(!card.answerable() && card.state_note == Some(strings::ANSWER_LOST_NOTE));
    assert_eq!(
        q.answer("a", Decision::AllowOnce, T0 + 900_002),
        Err(AnswerError::NotWaiting)
    );
    // The server's word replaces the guess and the note.
    resolved(&mut q, "a", "approved", T0 + 901_000);
    let card = q.card("a", T0 + 901_001).unwrap();
    assert_eq!(card.state, CardState::Decided(Outcome::Allowed));
    assert!(card.state_note.is_none());
}

#[test]
fn a_failed_send_gives_the_card_back_only_while_it_is_pending_and_in_time() {
    let mut q = known();
    requested(&mut q, pending("a", "s"), T0);
    requested(&mut q, pending("b", "s"), T0);
    q.answer("a", Decision::AllowOnce, T0 + 1).unwrap();
    q.answer("b", Decision::AllowOnce, T0 + 1).unwrap();
    q.apply(ApprovalsAction::AnswerFailed("a".into(), false, T0 + 2));
    assert_eq!(q.card("a", T0 + 2).unwrap().state, CardState::Waiting);
    q.apply(ApprovalsAction::AnswerFailed(
        "b".into(),
        false,
        T0 + 900_500,
    ));
    assert_eq!(outcome(&q, "b", T0 + 900_501), Some(Outcome::Expired));
    assert!(!q.card("b", T0 + 900_501).unwrap().answerable());
    // A request that is not being answered is not touched.
    assert!(!q.apply(ApprovalsAction::AnswerFailed("a".into(), false, T0 + 3)));
    resolved(&mut q, "a", "denied", T0 + 4);
    assert!(!q.apply(ApprovalsAction::AnswerFailed("a".into(), false, T0 + 5)));
    assert_eq!(outcome(&q, "a", T0 + 6), Some(Outcome::NotAllowed));
}

#[test]
fn a_trimmed_decided_request_is_never_brought_back_by_a_frame_or_a_snapshot() {
    let mut q = known();
    let n = MAX_DECIDED + 1_000;
    for i in 0..n {
        resolved(&mut q, &format!("d{i}"), "denied", T0 + i as u64);
    }
    assert_eq!(q.rows(Tab::Decided, T0 + 1_000_000).len(), MAX_DECIDED);
    for id in ["d0", "d999"] {
        assert!(!requested(&mut q, pending(id, "s"), T0 + 2_000_000));
        let mut row = pending(id, "s");
        row.status = "denied".into();
        snapshot(&mut q, vec![row], Scope::All, T0 + 2_000_001);
    }
    assert_eq!(q.waiting_count(), 0);
    assert_eq!(q.rows(Tab::Decided, T0 + 3_000_000).len(), MAX_DECIDED);
}

#[test]
fn a_snapshot_of_old_history_does_not_push_out_the_owners_own_decisions() {
    let mut q = known();
    requested(&mut q, pending("mine", "s"), T0);
    q.answer("mine", Decision::DontAllow, T0 + 1).unwrap();
    resolved(&mut q, "mine", "denied", T0 + 2);
    let rows = (0..300u64)
        .map(|i| {
            let mut r = pending(&format!("h{i}"), "s");
            r.status = "approved".into();
            r.created_at_ms = Some(T0 - 100_000 + i);
            r
        })
        .collect();
    snapshot(&mut q, rows, Scope::All, T0 + 5_000);
    let decided = q.rows(Tab::Decided, T0 + 6_000);
    assert_eq!(decided.len(), MAX_DECIDED);
    assert_eq!(decided[0].id, "mine", "the newest decision is first");
    // History is ordered by when the request was made, newest first.
    assert_eq!(decided[1].id, "h299");
}

#[test]
fn waiting_requests_are_capped_and_the_rest_is_counted_never_hidden() {
    let mut q = known();
    let rows: Vec<_> = (0..MAX_WAITING + 50)
        .map(|i| pending(&format!("w{i}"), &format!("s{}", i % 2)))
        .collect();
    snapshot(&mut q, rows, Scope::All, T0);
    assert_eq!(q.waiting_count(), MAX_WAITING);
    assert_eq!(q.waiting_at_least(), MAX_WAITING + 50);
    assert_eq!(q.waiting_badge(), Badge::Unknown, "never a smaller number");
    // A task whose request has no room still blocks its box.
    assert!(matches!(
        q.composer("s0"),
        ComposerState::WaitingForAnswer { .. }
    ));
    // The overflowing request is settled by the server like any other.
    let last = format!("w{}", MAX_WAITING + 49);
    resolved(&mut q, &last, "approved", T0 + 1);
    assert_eq!(q.waiting_at_least(), MAX_WAITING + 49);
    // Room again after a full list: the count is exact.
    q.apply(ApprovalsAction::FetchStarted(Scope::All, FetchId(1)));
    snapshot(&mut q, vec![pending("only", "s0")], Scope::All, T0 + 2);
    assert_eq!(q.waiting_badge(), Badge::Count(1));
    // Overflowing requests expire with their time.
    let mut q = known();
    let rows = (0..MAX_WAITING + 5).map(|i| pending(&format!("w{i}"), "s0"));
    snapshot(&mut q, rows.collect(), Scope::All, T0);
    q.apply(ApprovalsAction::Tick(T0 + 900_000));
    assert_eq!(q.waiting_at_least(), 0);
}

#[test]
fn rows_with_the_same_deadline_keep_their_arrival_order() {
    let mut q = known();
    for id in ["c", "a", "b", "e", "d"] {
        requested(&mut q, pending(id, "s"), T0);
    }
    let ids: Vec<_> = q.rows(Tab::Waiting, T0).into_iter().map(|r| r.id).collect();
    assert_eq!(ids, ["c", "a", "b", "e", "d"]);
}

#[test]
fn a_clock_near_the_end_of_time_does_not_panic() {
    let mut q = known();
    requested(&mut q, pending("a", "s"), u64::MAX - 10);
    assert_eq!(q.waiting_count(), 1);
}

#[test]
fn a_row_with_a_bad_id_is_refused_and_the_badge_says_so() {
    let mut q = known();
    let bad = ["", "a b", "a/b", "a\u{202e}b", &"a".repeat(129)];
    for id in bad {
        requested(&mut q, pending(id, "s"), T0);
    }
    requested(&mut q, pending("ok", "bad session!"), T0);
    assert_eq!((q.waiting_count(), q.refused()), (0, bad.len() + 1));
    assert_eq!(q.waiting_badge(), Badge::Unknown);
    assert_eq!(
        q.answer("", Decision::AllowOnce, T0),
        Err(AnswerError::Unknown)
    );
    // A refused decision is not a request nobody can answer.
    resolved(&mut q, "a b", "approved", T0);
    assert_eq!(q.refused(), bad.len() + 1);
    // A full list that was asked for after them is the truth again.
    q.apply(ApprovalsAction::FetchStarted(Scope::All, FetchId(1)));
    snapshot(&mut q, vec![pending("ok", "s")], Scope::All, T0 + 1);
    assert_eq!((q.refused(), q.waiting_badge()), (0, Badge::Count(1)));
    let row = &q.rows(Tab::Waiting, T0 + 1)[0];
    assert_eq!(
        (row.id.as_str(), row.session_id.as_deref()),
        ("ok", Some("s"))
    );
}

#[test]
fn a_status_this_version_does_not_know_is_corrected_by_a_known_one() {
    let mut q = known();
    resolved(&mut q, "a", "", T0);
    assert_eq!(outcome(&q, "a", T0 + 1), Some(Outcome::Closed));
    resolved(&mut q, "a", "approved", T0 + 2);
    assert_eq!(outcome(&q, "a", T0 + 3), Some(Outcome::Allowed));
    // A guess does not replace a decision, and a pending request with a
    // mangled status is hidden from answers (never allowed) but can be fixed.
    resolved(&mut q, "a", "???", T0 + 4);
    assert_eq!(outcome(&q, "a", T0 + 5), Some(Outcome::Allowed));
    requested(&mut q, ApprovalRequest::new("b", "t", "Pending"), T0);
    assert_eq!(q.waiting_count(), 0);
    resolved(&mut q, "b", "denied", T0 + 6);
    assert_eq!(outcome(&q, "b", T0 + 7), Some(Outcome::NotAllowed));
}

fn made_at(id: &str, t: u64) -> ApprovalRequest {
    let mut r = pending(id, "s");
    r.created_at_ms = Some(t);
    r
}

#[test]
fn the_selection_never_jumps_to_a_card_the_user_did_not_read() {
    let mut q = known();
    requested(&mut q, made_at("late", T0 + 30_000), T0 + 30_000);
    requested(&mut q, made_at("early", T0), T0 + 30_000);
    // A request that appears while nothing is selected is selected; a later
    // one does not take the selection.
    assert_eq!(q.selected(), Some("late"));
    // The selected request is decided: no card is armed, nothing else is.
    resolved(&mut q, "late", "approved", T0 + 40_000);
    assert_eq!((q.selected(), q.selected_card(T0).is_none()), (None, true));
    assert!(q.rows(Tab::Waiting, T0).iter().all(|r| !r.selected));
    // The same when the selected request expires on a tick.
    assert!(q.apply(ApprovalsAction::Select(Some("early".into()))));
    q.apply(ApprovalsAction::Tick(T0 + 900_000));
    assert_eq!(q.selected(), None);
    // A request of the other tab cannot be selected.
    requested(&mut q, made_at("c", T0 + 900_001), T0 + 900_001);
    assert_eq!(q.selected(), Some("c"));
    assert!(!q.apply(ApprovalsAction::Select(Some("late".into()))));
    assert_eq!(q.selected(), Some("c"));
    // Another tab, another list: the user's move shows its newest card.
    q.apply(ApprovalsAction::SelectTab(Tab::Decided));
    assert_eq!(q.selected(), Some("early"));
    q.apply(ApprovalsAction::SelectTab(Tab::Waiting));
    assert_eq!(q.selected(), Some("c"));
    // Deselecting stays deselected until a request appears.
    q.apply(ApprovalsAction::Select(None));
    assert_eq!(q.selected(), None);
    requested(&mut q, made_at("d", T0 + 900_002), T0 + 900_002);
    assert_eq!(q.selected(), Some("c"), "the one that waits longest");
}
