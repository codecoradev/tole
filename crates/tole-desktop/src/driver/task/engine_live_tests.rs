//! The stream, the queue of approvals and the owner's answers.

use std::io;

use serde_json::json;
use tole_client::Error;
use tole_desktop_model::approvals::Decision;
use tole_desktop_model::task::Phase;

use super::calls::{Call, Lane, Reply};
use super::engine::Input;
use super::fixtures::*;
use super::sim::Sim;
use super::view::LinkState;
use crate::adapter::task::{
    approvals_backoff, APPROVALS_RETRY_MAX, MAX_APPROVALS_RETRIES, MAX_REFRESH_ROWS,
};

fn io_error() -> Error {
    Error::Io(io::Error::other("reset"))
}

fn read_token(sim: &mut Sim) -> u64 {
    match sim.take(Lane::Read) {
        Call::Approvals { token } => token,
        _ => panic!("not an approvals call"),
    }
}

#[test]
fn the_first_open_reads_this_tasks_queue_once() {
    let mut sim = Sim::new(true);
    assert_eq!(sim.view().link, LinkState::Connecting);
    sim.up(false);
    assert_eq!(sim.view().link, LinkState::Live);
    sim.answer_approvals(vec![approval("a1", "pending"), other_session_approval("x")]);
    let v = sim.view();
    assert_eq!(
        v.approvals.len(),
        1,
        "another session's request is not shown"
    );
    assert_eq!(v.status.phase, Phase::WaitingApproval);
    sim.advance(600_000);
    assert!(sim.none_pending(), "read once, not on a timer");
}

#[test]
fn a_reconnect_reads_the_queue_again_and_a_read_from_before_it_is_dropped() {
    let mut sim = Sim::live();
    sim.stream(reconnected());
    let first = read_token(&mut sim);
    assert!(sim.view().approvals_stale);
    sim.stream(reconnected()); // another loss while the read is out
    sim.answer(Reply::Approvals(
        first,
        Ok(list(vec![approval("old", "pending")])),
    ));
    let v = sim.view();
    assert!(
        v.approvals.is_empty() && v.approvals_stale,
        "the old list is dropped"
    );
    sim.answer_approvals(vec![]);
    assert!(!sim.view().approvals_stale);
}

#[test]
fn a_failed_queue_read_is_retried_for_ever_at_the_adapters_pace() {
    let mut sim = Sim::live();
    sim.stream(reconnected());
    for attempt in 0..(MAX_APPROVALS_RETRIES + 4) {
        let token = read_token(&mut sim);
        sim.answer(Reply::Approvals(token, Err(io_error())));
        let wait = approvals_backoff(attempt).unwrap_or(APPROVALS_RETRY_MAX);
        let ms = wait.as_millis() as u64;
        sim.advance(ms - 1);
        assert!(
            !sim.has(Lane::Read),
            "attempt {attempt}: not before {ms} ms"
        );
        sim.advance(1);
        assert!(sim.has(Lane::Read), "attempt {attempt}: at {ms} ms");
    }
    assert!(sim.view().approvals_stale);
}

#[test]
fn a_queue_too_long_to_be_one_is_a_failed_read() {
    let mut sim = Sim::live();
    sim.stream(reconnected());
    let rows = (0..=MAX_REFRESH_ROWS).map(|n| approval(&format!("a{n}"), "pending"));
    let token = read_token(&mut sim);
    sim.answer(Reply::Approvals(token, Ok(list(rows.collect()))));
    let v = sim.view();
    assert!(v.approvals.is_empty() && v.approvals_stale);
    sim.advance(2_000);
    assert!(sim.has(Lane::Read), "asked again, not believed");
}

#[test]
fn a_lost_stream_reads_offline_and_a_reopened_one_is_live_again_and_reads_the_queue() {
    let mut sim = Sim::live();
    sim.stream_error(io_error());
    assert_eq!(
        (sim.phase(), sim.view().link),
        (Phase::Offline, LinkState::Lost)
    );
    sim.up(true);
    assert_eq!(
        (sim.phase(), sim.view().link),
        (Phase::Idle, LinkState::Live)
    );
    assert!(sim.view().approvals_stale);
    sim.answer_approvals(vec![]);
    assert!(!sim.view().approvals_stale);
}

#[test]
fn a_parked_stream_is_flagged_until_one_opens() {
    let mut sim = Sim::live();
    sim.feed(Input::Parked);
    assert!(sim.view().parked);
    sim.up(true);
    assert!(!sim.view().parked);
}

#[test]
fn a_closed_session_asks_nothing_more() {
    let mut sim = Sim::live();
    sim.stream(frame("closed", json!({"id": S})));
    assert_eq!(sim.phase(), Phase::Closed);
    sim.stream(reconnected());
    sim.suspend(60_000);
    sim.advance(60_000);
    assert!(sim.none_pending());
}

fn asking_two() -> Sim {
    let mut sim = Sim::live();
    sim.stream(requested("a1"));
    sim.stream(requested("a2"));
    sim
}

fn decide_call(sim: &mut Sim) -> (String, Decision) {
    match sim.take(Lane::Decide) {
        Call::Decide { id, decision } => (id, decision),
        _ => panic!("not a decision call"),
    }
}

#[test]
fn answers_go_one_at_a_time_and_only_for_cards_the_task_shows() {
    let mut sim = asking_two();
    sim.decide("nope", Decision::AllowOnce);
    assert!(sim.none_pending());
    sim.decide("a1", Decision::AllowOnce);
    sim.decide("a2", Decision::DontAllow);
    sim.decide("a1", Decision::DontAllow);
    assert_eq!(decide_call(&mut sim), ("a1".into(), Decision::AllowOnce));
    assert!(!sim.has(Lane::Decide), "a2 waits for a1's answer");
    assert_eq!(sim.view().deciding, vec!["a1", "a2"]);
    sim.answer(Reply::Decide("a1".into(), Ok(decided("a1", "approved"))));
    assert_eq!(sim.view().approvals.len(), 1, "a2 still waits");
    assert_eq!(decide_call(&mut sim), ("a2".into(), Decision::DontAllow));
    sim.answer(Reply::Decide("a2".into(), Ok(decided("a2", "denied"))));
    let v = sim.view();
    assert!(v.approvals.is_empty() && v.deciding.is_empty());
    assert_eq!(
        v.status.phase,
        Phase::Idle,
        "a denial leaves the turn parked"
    );
}

#[test]
fn a_queued_answer_is_checked_again_when_it_is_due_to_leave() {
    let mut sim = asking_two();
    sim.decide("a1", Decision::AllowOnce);
    sim.decide("a2", Decision::AllowOnce);
    decide_call(&mut sim);
    // Another face answers a2 while its answer waits.
    sim.stream(resolved("a2", "denied"));
    sim.answer(Reply::Decide("a1".into(), Ok(decided("a1", "approved"))));
    assert!(!sim.has(Lane::Decide), "a2 is gone: nothing is sent for it");
    assert!(sim.view().deciding.is_empty());
}

#[test]
fn an_allow_resumes_the_turn_and_the_status_is_polled() {
    let mut sim = Sim::live();
    sim.stream(requested("a1"));
    sim.decide("a1", Decision::AllowOnce);
    decide_call(&mut sim);
    sim.answer(Reply::Decide("a1".into(), Ok(decided("a1", "approved"))));
    assert_eq!(sim.phase(), Phase::Resuming);
    // The stream's own word for it changes nothing.
    sim.stream(resolved("a1", "approved"));
    assert_eq!(sim.phase(), Phase::Resuming);
    sim.advance(500);
    assert!(sim.has(Lane::Status));
}

#[test]
fn the_serves_word_is_what_the_model_hears_even_when_it_differs_from_the_press() {
    let mut sim = Sim::live();
    sim.stream(requested("a1"));
    sim.decide("a1", Decision::AllowOnce);
    decide_call(&mut sim);
    sim.answer(Reply::Decide("a1".into(), Ok(decided("a1", "expired"))));
    assert_eq!(sim.phase(), Phase::Idle, "expired is not an allow");
}

#[test]
fn an_answer_that_may_not_have_landed_reads_the_queue_again() {
    for reply in [
        Err(io_error()),
        Err(status_code(409)),
        Ok(decided("other", "approved")),
        Ok(decided("a1", "pending")),
    ] {
        let mut sim = Sim::live();
        sim.stream(requested("a1"));
        sim.decide("a1", Decision::AllowOnce);
        decide_call(&mut sim);
        sim.answer(Reply::Decide("a1".into(), reply));
        assert_eq!(sim.phase(), Phase::WaitingApproval, "no allow was believed");
        // The queue is read; the stream's own announcement is kept (the
        // model's rule), so the card goes when its `resolved` frame comes.
        sim.answer_approvals(vec![approval("a1", "pending")]);
        assert_eq!(sim.view().approvals.len(), 1);
        sim.stream(resolved("a1", "denied"));
        assert_eq!(sim.phase(), Phase::Idle);
    }
}
