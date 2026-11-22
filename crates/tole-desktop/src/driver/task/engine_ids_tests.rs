//! The serve's own ids of approvals: answered under the raw id, and a card
//! that cannot be answered says so and still lets the owner stop the turn.

use std::io;

use tole_client::Error;
use tole_desktop_model::approvals::Decision;
use tole_desktop_model::task::Phase;

use super::calls::{Call, Lane, Reply};
use super::engine::Input;
use super::fixtures::*;
use super::sim::Sim;

/// An id the model shows in another form: a control character becomes a space.
const BELL: &str = "x\u{7}1";

fn decide_call(sim: &mut Sim) -> String {
    match sim.take(Lane::Decide) {
        Call::Decide { id, .. } => id,
        _ => panic!("not a decision call"),
    }
}

#[test]
fn a_card_is_answered_under_the_serves_own_id_not_the_one_the_model_shows() {
    let mut sim = Sim::live();
    sim.stream(requested(BELL));
    assert_eq!(sim.view().approvals[0].id, "x 1", "what the owner reads");
    assert!(sim.view().undecidable.is_empty());
    sim.decide("x 1", Decision::AllowOnce);
    assert_eq!(decide_call(&mut sim), BELL, "what the serve knows");
    sim.answer(Reply::Decide(BELL.into(), Ok(decided(BELL, "approved"))));
    assert_eq!(sim.phase(), Phase::Resuming, "the card is the one answered");
}

#[test]
fn an_id_past_the_models_cut_is_still_answerable() {
    let mut sim = Sim::live();
    let long = "z".repeat(100);
    sim.stream(requested(&long));
    let shown = sim.view().approvals[0].id.clone();
    assert_ne!(shown, long);
    sim.decide(&shown, Decision::DontAllow);
    assert_eq!(decide_call(&mut sim), long);
    sim.answer(Reply::Decide(long.clone(), Ok(decided(&long, "denied"))));
    assert_eq!(sim.phase(), Phase::Idle);
}

#[test]
fn an_id_from_the_queue_read_is_known_too() {
    let mut sim = Sim::live();
    sim.stream(reconnected());
    sim.answer_approvals(vec![approval(BELL, "pending")]);
    sim.decide("x 1", Decision::AllowOnce);
    assert_eq!(decide_call(&mut sim), BELL);
}

#[test]
fn two_requests_that_look_the_same_are_one_card_nobody_can_answer_from_here() {
    let mut sim = Sim::live();
    sim.prompt("tidy");
    sim.entry(&user(1, "tidy"));
    sim.stream(requested("a1"));
    sim.stream(requested("a1 "));
    let v = sim.view();
    assert_eq!(
        (v.approvals.len(), v.undecidable.clone()),
        (1, vec!["a1".to_string()])
    );
    sim.decide("a1", Decision::AllowOnce);
    assert!(!sim.has(Lane::Decide), "which of the two would it be?");
    assert_eq!(sim.view().ignored, 1);
    // The model offers no Stop while a card waits: the driver does.
    assert_eq!(sim.phase(), Phase::WaitingApproval);
    assert!(sim.view().status.can_stop);
}

#[test]
fn stop_on_a_card_nobody_can_answer_cancels_the_turn_and_reads_the_queue_again() {
    let mut sim = Sim::live();
    sim.stream(requested("a1"));
    sim.stream(requested("a1 "));
    sim.feed(Input::Stop);
    assert!(matches!(sim.take(Lane::Stop), Call::Cancel));
    sim.answer(Reply::Cancel(Ok(cancelled(true))));
    let v = sim.view();
    assert!(v.approvals.is_empty() && v.undecidable.is_empty());
    assert_eq!(v.status.phase, Phase::Idle);
    // The model does not bring back a card it was told was decided, whatever
    // the queue says; a reconnect starts its memory over and the queue is read.
    sim.answer_approvals(vec![approval("a1", "pending")]);
    assert!(sim.view().approvals.is_empty());
    sim.stream(reconnected());
    sim.answer_approvals(vec![approval("a1", "pending")]);
    assert_eq!(sim.view().approvals.len(), 1, "still pending on the serve");
    assert!(sim.view().undecidable.is_empty(), "one id now: answerable");
}

#[test]
fn a_failed_cancel_leaves_the_cards_where_they_are() {
    let mut sim = Sim::live();
    sim.stream(requested("a1"));
    sim.stream(requested("a1 "));
    sim.feed(Input::Stop);
    sim.take(Lane::Stop);
    sim.answer(Reply::Cancel(Err(Error::Io(io::Error::other("reset")))));
    assert_eq!(sim.view().approvals.len(), 1);
    assert!(sim.view().status.can_stop, "Stop is still offered");
}

#[test]
fn stop_on_a_card_that_can_be_answered_is_the_cards_job() {
    let mut sim = Sim::live();
    sim.stream(requested("a1"));
    sim.feed(Input::Stop);
    assert!(sim.none_pending());
    assert!(!sim.view().status.can_stop);
    assert_eq!(sim.view().ignored, 1);
}
