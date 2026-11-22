//! Stop, in every outcome the model's docs describe.

use std::io;

use tole_client::Error;
use tole_desktop_model::task::{Phase, SendMode};

use super::calls::{Call, Lane, Reply};
use super::engine::Input;
use super::fixtures::*;
use super::sim::Sim;
use crate::adapter::task::MAX_STOP_POLLS;

fn running() -> Sim {
    let mut sim = Sim::live();
    sim.prompt("tidy");
    sim.entry(&user(1, "tidy"));
    sim
}

fn cancel_call(sim: &mut Sim) {
    assert!(matches!(sim.take(Lane::Stop), Call::Cancel));
}

#[test]
fn stop_before_the_call_went_out_takes_the_send_back_and_cancels_nothing() {
    let mut sim = Sim::live();
    let gate = sim.prompt_unstarted("tidy");
    sim.feed(Input::Stop);
    assert!(
        !sim.has(Lane::Stop),
        "there is nothing on the serve to cancel"
    );
    assert_eq!(sim.phase(), Phase::Idle);
    assert_eq!(sim.view().draft, "tidy", "the model has its words back");
    assert!(!gate.start(), "the lane thread finds the send taken back");
    // The thread answers that no call was made; only then is sending open.
    assert_eq!(sim.send_mode(), SendMode::Disabled(Phase::Working));
    sim.answer(Reply::Prompt(sim.generation, None));
    assert_eq!(sim.send_mode(), SendMode::Send);
}

#[test]
fn stop_after_the_call_started_is_a_cancel_and_nothing_stacks() {
    let mut sim = running();
    sim.feed(Input::Stop);
    cancel_call(&mut sim);
    assert_eq!(sim.phase(), Phase::Stopping);
    sim.feed(Input::Stop);
    assert!(sim.none_pending(), "already asked");
    sim.answer(Reply::Cancel(Ok(cancelled(false))));
    assert_eq!(sim.phase(), Phase::Idle, "nothing ran: honoured at once");
}

#[test]
fn a_cancel_that_is_accepted_while_the_turn_keeps_running_offers_stop_again() {
    let mut sim = running();
    sim.feed(Input::Stop);
    cancel_call(&mut sim);
    sim.answer(Reply::Cancel(Ok(cancelled(true))));
    assert_eq!(sim.phase(), Phase::Stopping);
    for n in 1..=MAX_STOP_POLLS {
        sim.advance(500);
        sim.status_answer(true);
        let can = sim.view().status.can_stop;
        assert_eq!(can, n >= MAX_STOP_POLLS, "poll {n}");
    }
    assert_eq!(sim.phase(), Phase::Working);
    // Offered again, and the next Stop is a new cancel.
    sim.feed(Input::Stop);
    cancel_call(&mut sim);
    // The watch goes on at the slow pace: the turn that ends later ends here.
    sim.answer(Reply::Cancel(Ok(cancelled(true))));
    sim.advance(5_000);
    sim.status_answer(false);
    assert_eq!(sim.phase(), Phase::Idle);
}

#[test]
fn a_failed_cancel_offers_stop_again_and_the_turn_runs_on() {
    let mut sim = running();
    sim.feed(Input::Stop);
    cancel_call(&mut sim);
    sim.answer(Reply::Cancel(Err(Error::Io(io::Error::other("reset")))));
    assert_eq!(sim.phase(), Phase::Working);
    assert!(sim.view().status.can_stop);
}

#[test]
fn a_stop_during_a_resumed_turn_is_watched_until_the_server_is_idle() {
    let mut sim = running();
    sim.stream(requested("a1"));
    sim.stream(resolved("a1", "approved"));
    assert_eq!(sim.phase(), Phase::Resuming);
    sim.feed(Input::Stop);
    cancel_call(&mut sim);
    sim.answer(Reply::Cancel(Ok(cancelled(true))));
    sim.advance(500);
    sim.status_answer(false);
    assert_eq!(
        sim.phase(),
        Phase::Idle,
        "a not-busy answer after a Stop is final"
    );
}

#[test]
fn a_cancel_reply_after_the_turn_ended_starts_no_watch() {
    let mut sim = running();
    sim.feed(Input::Stop);
    cancel_call(&mut sim);
    sim.entry(&answer(2, "done"));
    sim.answer(Reply::Cancel(Ok(cancelled(true))));
    sim.advance(60_000);
    assert!(!sim.has(Lane::Status));
}
