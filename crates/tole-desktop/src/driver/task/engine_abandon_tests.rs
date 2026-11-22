//! A prompt call that never answers is abandoned; and three guards a mutation
//! run showed no test was holding.

use std::io;

use tole_client::Error;
use tole_desktop_model::approvals::Decision;
use tole_desktop_model::task::{Phase, SendMode};

use super::calls::{Call, Lane, Reply};
use super::engine::Input;
use super::fixtures::*;
use super::sim::Sim;

const HOUR_MS: u64 = 3_600_000;

fn io_error() -> Error {
    Error::Io(io::Error::other("reset"))
}

/// A turn that ended on the stream while its prompt call is still out.
pub(super) fn ended_with_the_call_out() -> Sim {
    let mut sim = Sim::live();
    sim.prompt("tidy");
    sim.entry(&user(1, "tidy"));
    sim.entry(&answer(2, "done"));
    assert_eq!(sim.phase(), Phase::Idle);
    sim
}

#[test]
fn a_call_that_never_answers_is_abandoned_and_sending_opens_again_within_three_hours() {
    let mut sim = ended_with_the_call_out();
    let first = sim.generation;
    assert_eq!(sim.send_mode(), SendMode::Disabled(Phase::Working));
    sim.advance(29_000);
    assert_eq!(
        sim.send_mode(),
        SendMode::Disabled(Phase::Working),
        "not yet"
    );
    for _ in 0..(3 * HOUR_MS / 10_000) {
        sim.advance(10_000);
    }
    assert_eq!(sim.send_mode(), SendMode::Send, "three hours on");
    assert_eq!(
        sim.replaced,
        vec![Lane::Prompt],
        "once, and a thread is asked for"
    );
    assert_eq!(sim.prompt("again"), "again", "the new call goes out");
    assert_ne!(sim.generation, first);
}

#[test]
fn the_late_answer_of_an_abandoned_call_is_dropped() {
    let mut sim = ended_with_the_call_out();
    let first = sim.generation;
    sim.advance(31_000);
    sim.prompt("again");
    sim.entry(&user(3, "again"));
    assert_eq!(sim.phase(), Phase::Working);
    // The old call wakes up at last and says its turn finished.
    sim.answer(Reply::Prompt(first, Some(Ok(finished()))));
    assert_eq!(sim.phase(), Phase::Working, "it does not end the new turn");
    sim.feed(Input::Send("third".into()));
    assert!(!sim.has(Lane::Prompt), "nor free the new call's lane");
    sim.answer(Reply::Prompt(first, Some(Err(io_error()))));
    assert_eq!(sim.view().draft, "", "nor give back words it never held");
    sim.answer_prompt(Ok(finished()));
    assert_eq!(sim.send_mode(), SendMode::Send);
}

#[test]
fn after_a_suspend_the_call_is_given_up_in_seconds() {
    let mut sim = ended_with_the_call_out();
    sim.suspend(60_000);
    assert_eq!(sim.send_mode(), SendMode::Disabled(Phase::Working));
    sim.advance(2_900);
    assert!(sim.replaced.is_empty());
    sim.advance(100);
    assert_eq!(sim.send_mode(), SendMode::Send);
}

#[test]
fn a_turn_that_runs_is_never_given_up_on() {
    let mut sim = Sim::live();
    sim.prompt("tidy");
    sim.entry(&user(1, "tidy"));
    for _ in 0..(3 * HOUR_MS / 10_000) {
        sim.advance(10_000);
    }
    sim.suspend(60_000);
    sim.advance(60_000);
    assert!(sim.replaced.is_empty(), "the call is how this turn ends");
    assert_eq!(sim.phase(), Phase::Working);
}

#[test]
fn a_stop_that_took_the_send_back_is_not_an_abandonment() {
    let mut sim = Sim::live();
    let gate = sim.prompt_unstarted("tidy");
    sim.feed(Input::Stop);
    assert!(!gate.start());
    sim.advance(120_000);
    assert!(sim.replaced.is_empty(), "the thread answers `no call made`");
}

#[test]
fn no_foreign_watcher_runs_while_the_prompt_call_is_out() {
    let mut sim = Sim::live();
    sim.prompt("tidy");
    sim.stream(ready(true));
    sim.advance(2_000);
    assert!(
        !sim.has(Lane::Status),
        "the call will report the end of this turn"
    );
}

#[test]
fn an_older_status_check_cannot_answer_a_newer_one() {
    let mut sim = Sim::live();
    sim.prompt("a");
    sim.answer_prompt(Err(io_error()));
    let (token, check) = match sim.take(Lane::Status) {
        Call::Status { token, check } => (token, check),
        _ => panic!("not a status call"),
    };
    // The owner sends again, and that fails too: a second check is wanted.
    sim.prompt("b");
    sim.answer_prompt(Err(io_error()));
    assert_eq!(sim.phase(), Phase::Idle);
    sim.answer(Reply::Status {
        token,
        check,
        reply: Ok(status(true)),
    });
    assert_eq!(
        sim.phase(),
        Phase::Idle,
        "the first check is about the first send"
    );
    sim.status_answer(false);
    assert_eq!(sim.phase(), Phase::Idle);
}

#[test]
fn the_same_card_is_not_queued_twice() {
    let mut sim = Sim::live();
    sim.stream(requested("a1"));
    sim.stream(requested("a2"));
    sim.decide("a1", Decision::AllowOnce);
    sim.decide("a2", Decision::AllowOnce);
    sim.decide("a2", Decision::DontAllow);
    assert_eq!(
        sim.view().deciding,
        vec!["a1", "a2"],
        "a2 once, as first chosen"
    );
    assert_eq!(sim.view().ignored, 1);
}
