//! Recovery of the queue (#1116): refusals and fetches that never end, stale
//! replies, the order of simultaneous expiries, conflicts.

use super::fixtures::{req, T0};
use super::wait::SKEW_MS;
use super::*;
use crate::skills::Badge;

const WAIT_MS: u64 = EXPIRY_SECS * 1000;
const TIMEOUT_MS: u64 = FETCH_TIMEOUT_SECS * 1000;

fn requested(q: &mut ApprovalsState, r: ApprovalRequest, now: u64) -> bool {
    q.apply(ApprovalsAction::Requested(r, now))
}

fn started(q: &mut ApprovalsState, scope: Scope, id: u64) {
    q.apply(ApprovalsAction::FetchStarted(scope, FetchId(id)));
}

fn reply(q: &mut ApprovalsState, rows: Vec<ApprovalRequest>, scope: Scope, id: u64, now: u64) {
    q.apply(ApprovalsAction::Snapshot(rows, scope, FetchId(id), now));
}

fn tick(q: &mut ApprovalsState, now: u64) {
    q.apply(ApprovalsAction::Tick(now));
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

fn lost_then_fetching(id: u64) -> ApprovalsState {
    let mut q = known();
    q.apply(ApprovalsAction::ConnectionLost);
    started(&mut q, Scope::All, id);
    q
}

fn decided_ids(q: &ApprovalsState) -> Vec<String> {
    q.rows(Tab::Decided, T0).into_iter().map(|r| r.id).collect()
}

#[test]
fn a_refused_frame_stops_counting_when_the_request_cannot_wait_any_more() {
    let mut q = known();
    requested(&mut q, pending("a", "bad/session"), T0);
    assert_eq!(q.waiting_badge(), Badge::Unknown);
    assert_eq!(q.composer("s"), ComposerState::Checking);
    tick(&mut q, T0 + WAIT_MS + SKEW_MS - 1);
    assert_eq!((q.refused(), q.waiting_badge()), (1, Badge::Unknown));
    assert!(q.apply(ApprovalsAction::Tick(T0 + WAIT_MS + SKEW_MS)));
    assert_eq!((q.refused(), q.waiting_badge()), (0, Badge::Count(0)));
    assert_eq!(q.composer("s"), ComposerState::Open);
}

#[test]
fn a_cut_guess_stops_counting_when_its_own_wait_is_over() {
    let mut q = known();
    for i in 0..=MAX_DECIDED {
        let mut r = pending(&format!("g{i}"), "s");
        r.status = "awaiting".into();
        requested(&mut q, r, T0);
    }
    assert_eq!(
        q.refused(),
        1,
        "one guess was cut and may have been pending"
    );
    tick(&mut q, T0 + WAIT_MS + SKEW_MS - 1);
    assert_eq!(q.waiting_badge(), Badge::Unknown, "with the skew as grace");
    tick(&mut q, T0 + WAIT_MS + SKEW_MS);
    assert_eq!((q.refused(), q.waiting_badge()), (0, Badge::Count(0)));
    assert_eq!(q.composer("s"), ComposerState::Open);
}

#[test]
fn a_fetch_that_never_answers_is_given_up_and_asks_for_a_new_one() {
    let mut q = lost_then_fetching(2);
    tick(&mut q, T0); // the clock of the fetch starts here
    assert_eq!(q.composer("s"), ComposerState::Checking);
    tick(&mut q, T0 + TIMEOUT_MS - 1);
    assert!(!q.needs_fetch());
    assert!(q.apply(ApprovalsAction::Tick(T0 + TIMEOUT_MS)));
    assert!(q.needs_fetch());
    assert_eq!(q.waiting_badge(), Badge::Unknown, "still not known");
    // The reply that comes after the give-up is not trusted.
    reply(&mut q, vec![], Scope::All, 2, T0 + TIMEOUT_MS + 1);
    assert_eq!(q.composer("s"), ComposerState::Checking);
    started(&mut q, Scope::All, 3);
    assert!(!q.needs_fetch(), "a fetch runs");
    reply(&mut q, vec![], Scope::All, 3, T0 + TIMEOUT_MS + 2);
    assert!(!q.needs_fetch());
    assert_eq!(q.composer("s"), ComposerState::Open);
}

#[test]
fn a_clock_that_steps_back_does_not_freeze_the_fetch_watchdog() {
    let mut q = lost_then_fetching(2);
    tick(&mut q, T0 + 10 * TIMEOUT_MS);
    tick(&mut q, T0); // back in time: the fetch is timed from here
    tick(&mut q, T0 + TIMEOUT_MS);
    assert!(q.needs_fetch());
}

#[test]
fn leaked_session_fetches_do_not_block_the_next_one() {
    let mut q = known();
    for i in 0..300 {
        started(&mut q, Scope::Session(format!("leak{i}")), 10 + i);
    }
    let late = Scope::Session("late".into());
    q.apply(ApprovalsAction::ConnectionLost);
    for i in 0..300 {
        started(&mut q, Scope::Session(format!("leak{i}")), 1_000 + i);
    }
    started(&mut q, late.clone(), 9_999);
    reply(&mut q, vec![], late, 9_999, T0);
    assert_eq!(q.composer("late"), ComposerState::Open);
    // And what nobody answers is pruned by the clock.
    tick(&mut q, T0);
    tick(&mut q, T0 + TIMEOUT_MS);
    assert!(q.needs_fetch());
}

#[test]
fn a_stale_reply_does_not_leave_a_request_the_current_one_cannot_close() {
    let mut q = lost_then_fetching(2);
    reply(&mut q, vec![pending("p", "s")], Scope::All, 1, T0);
    assert_eq!(
        q.waiting_count(),
        0,
        "an untrusted reply creates no waiting entry"
    );
    reply(&mut q, vec![], Scope::All, 2, T0 + 1);
    assert_eq!(q.waiting_badge(), Badge::Count(0));
    assert_eq!(
        q.answer("p", Decision::AllowOnce, T0 + 2),
        Err(AnswerError::Unknown)
    );
    // The current reply still shows what the server says is pending.
    let mut q = lost_then_fetching(2);
    reply(&mut q, vec![pending("p", "s")], Scope::All, 1, T0);
    reply(&mut q, vec![pending("p", "s")], Scope::All, 2, T0 + 1);
    assert_eq!(q.waiting_badge(), Badge::Count(1));
}

#[test]
fn simultaneous_closes_and_expiries_are_ranked_in_arrival_order() {
    let ids: Vec<String> = (0..30).map(|i| format!("p{i:02}")).collect();
    let newest_first: Vec<String> = ids.iter().rev().cloned().collect();
    let mut closed = known();
    let mut expired = known();
    for id in &ids {
        requested(&mut closed, pending(id, "s"), T0);
        requested(&mut expired, pending(id, "s"), T0);
    }
    started(&mut closed, Scope::All, 2);
    reply(&mut closed, vec![], Scope::All, 2, T0 + 1);
    assert_eq!(decided_ids(&closed), newest_first);
    tick(&mut expired, T0 + WAIT_MS);
    assert_eq!(decided_ids(&expired), newest_first);
}

#[test]
fn a_refusal_between_two_marks_asks_for_a_refetch() {
    let mut q = known();
    started(&mut q, Scope::All, 2);
    requested(&mut q, pending("x", "bad/session"), T0);
    started(&mut q, Scope::All, 3);
    reply(&mut q, vec![], Scope::All, 3, T0 + 1);
    assert_eq!(
        q.waiting_badge(),
        Badge::Unknown,
        "the older mark cannot account for it"
    );
    assert!(q.needs_fetch());
    started(&mut q, Scope::All, 4);
    assert!(!q.needs_fetch());
    reply(&mut q, vec![], Scope::All, 4, T0 + 2);
    assert_eq!(
        (q.waiting_badge(), q.needs_fetch()),
        (Badge::Count(0), false)
    );
    // A single fetch never asks.
    started(&mut q, Scope::All, 5);
    reply(&mut q, vec![], Scope::All, 5, T0 + 3);
    assert!(!q.needs_fetch());
}

#[test]
fn an_older_reply_does_not_revive_a_request_after_a_conflict_nor_clear_its_stale_flag() {
    let mut q = known();
    requested(&mut q, pending("p", "s"), T0);
    started(&mut q, Scope::All, 2);
    q.answer("p", Decision::AllowOnce, T0 + 1).unwrap();
    q.apply(ApprovalsAction::AnswerFailed("p".into(), true, T0 + 2));
    assert_eq!(q.waiting_count(), 0);
    reply(&mut q, vec![pending("p", "s")], Scope::All, 2, T0 + 3);
    assert_eq!(q.waiting_count(), 0, "the reply is older than the conflict");
    assert!(q.is_stale(), "and it did not settle it");
    started(&mut q, Scope::All, 3);
    reply(&mut q, vec![pending("p", "s")], Scope::All, 3, T0 + 4);
    assert_eq!((q.waiting_count(), q.is_stale()), (1, false));
    // A reply that answers no fetch revives nothing either.
    q.answer("p", Decision::DontAllow, T0 + 5).unwrap();
    q.apply(ApprovalsAction::AnswerFailed("p".into(), true, T0 + 6));
    reply(&mut q, vec![pending("p", "s")], Scope::All, 77, T0 + 7);
    assert_eq!(q.waiting_count(), 0);
}

#[test]
fn an_answer_refused_as_expired_has_already_moved_the_request() {
    let mut q = known();
    requested(&mut q, pending("p", "s"), T0);
    let now = T0 + WAIT_MS;
    assert_eq!(
        q.answer("p", Decision::AllowOnce, now),
        Err(AnswerError::Expired)
    );
    assert_eq!(decided_ids(&q), ["p"]);
    assert_eq!(q.waiting_count(), 0);
}

fn unreadable(id: &str) -> ApprovalRequest {
    let mut r = pending(id, "s");
    r.status = "awaiting".into();
    r
}

#[test]
fn a_guess_that_entered_after_the_fetch_started_is_revived_by_its_reply() {
    let mut q = known();
    started(&mut q, Scope::All, 2);
    requested(&mut q, unreadable("r"), T0);
    assert_eq!(q.composer("s"), ComposerState::Checking);
    reply(&mut q, vec![pending("r", "s")], Scope::All, 2, T0 + 1);
    assert_eq!(q.waiting_badge(), Badge::Count(1));
    assert!(q.answer("r", Decision::AllowOnce, T0 + 2).is_ok());
}

#[test]
fn a_malformed_row_of_a_stale_reply_leaves_no_refusal_the_current_one_cannot_clear() {
    let mut q = lost_then_fetching(2);
    let stale = vec![pending("p", "bad/session")];
    reply(&mut q, stale, Scope::All, 1, T0);
    assert_eq!(q.refused(), 0);
    reply(&mut q, vec![], Scope::All, 2, T0 + 1);
    assert_eq!(q.waiting_badge(), Badge::Count(0));
    assert_eq!(q.composer("s"), ComposerState::Open);
    // With no fetch running the row is refused as before, and the next fetch
    // settles it.
    let mut q = known();
    reply(&mut q, vec![pending("p", "bad/session")], Scope::All, 9, T0);
    assert_eq!(q.refused(), 1);
    started(&mut q, Scope::All, 2);
    reply(&mut q, vec![], Scope::All, 2, T0 + 1);
    assert_eq!((q.refused(), q.waiting_badge()), (0, Badge::Count(0)));
}

#[test]
fn a_reply_that_is_not_a_fetch_merges_its_rows_when_none_runs() {
    let mut q = known();
    reply(&mut q, vec![pending("p", "s")], Scope::All, 9, T0);
    assert_eq!(q.waiting_count(), 1);
    // The next fetch closes it when it omits it: it was seen before the start.
    started(&mut q, Scope::All, 2);
    reply(&mut q, vec![], Scope::All, 2, T0 + 1);
    assert_eq!((q.waiting_count(), q.waiting_badge()), (0, Badge::Count(0)));
}
