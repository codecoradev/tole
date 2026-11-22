//! Second review: what a repaint is told, the link while no stream is up, a
//! queued answer, the words of an abandoned call, and guards a mutation run
//! showed no test was holding.

use std::io;

use tole_client::Error;
use tole_desktop_model::approvals::Decision;
use tole_desktop_model::task::{Phase, SendMode};

use super::calls::{Call, Lane, Reply};
use super::engine::Input;
use super::engine_abandon_tests::ended_with_the_call_out;
use super::fixtures::*;
use super::sim::Sim;
use super::view::LinkState;

fn io_error() -> Error {
    Error::Io(io::Error::other("reset"))
}

/// A task that showed card `a1` and whose stream then failed: the next read
/// comes 5 s later, over REST.
fn card_then_no_stream() -> Sim {
    let mut sim = Sim::live();
    sim.stream(reconnected());
    sim.answer_approvals(vec![approval("a1", "pending")]);
    sim.stream_error(io_error());
    sim.advance(5_000);
    assert!(sim.has(Lane::Read));
    sim
}

#[test]
fn a_queue_read_that_swaps_one_card_for_another_repaints() {
    let mut sim = card_then_no_stream();
    let before = sim.changes;
    sim.answer_approvals(vec![approval("a2", "pending")]);
    assert_eq!(sim.view().approvals[0].id, "a2");
    assert!(sim.changes > before, "the card the owner can press changed");
}

#[test]
fn a_queue_read_that_makes_a_card_unanswerable_repaints() {
    let mut sim = card_then_no_stream();
    let before = sim.changes;
    sim.answer_approvals(vec![approval("a1", "pending"), approval("a1 ", "pending")]);
    assert_eq!(sim.view().undecidable, vec!["a1"]);
    assert!(sim.changes > before);
}

#[test]
fn a_rest_read_does_not_make_a_task_without_a_stream_look_connected() {
    let mut sim = Sim::live();
    sim.stream_error(io_error());
    assert_eq!(sim.view().link, LinkState::Lost);
    sim.advance(5_000);
    sim.status_answer(false);
    let v = sim.view();
    assert_eq!(
        v.link,
        LinkState::Lost,
        "the model's link flips, the view's does not"
    );
    assert_eq!(
        v.status.send,
        SendMode::Disabled(Phase::Offline),
        "no answer could come"
    );
    sim.feed(Input::Send("hello".into()));
    assert!(!sim.has(Lane::Prompt), "nor does the engine send");
    // A stream item that arrives is what ends it.
    sim.up(true);
    assert_eq!(
        (sim.view().link, sim.send_mode()),
        (LinkState::Live, SendMode::Send)
    );
}

#[test]
fn a_parked_task_is_lost_not_live_and_stop_still_works_on_what_a_read_found() {
    let mut sim = Sim::live();
    sim.stream_error(io_error());
    sim.feed(Input::Parked);
    sim.advance(5_000);
    sim.status_answer(true);
    let v = sim.view();
    assert_eq!((v.link, v.parked), (LinkState::Lost, true));
    assert!(
        v.status.can_stop,
        "a turn runs: Stop is the serve's to cancel"
    );
    sim.feed(Input::Stop);
    assert!(matches!(sim.take(Lane::Stop), Call::Cancel));
}

#[test]
fn a_task_whose_stream_never_came_up_cannot_send_either() {
    let mut sim = Sim::new(true);
    assert_eq!(sim.view().link, LinkState::Connecting);
    sim.feed(Input::Send("early".into()));
    assert!(sim.none_pending());
    assert_eq!(sim.view().ignored, 1);
}

#[test]
fn a_queued_answer_is_not_sent_when_its_request_is_no_longer_certain() {
    let mut sim = Sim::live();
    sim.stream(requested("a1"));
    sim.stream(requested("a2"));
    sim.decide("a1", Decision::AllowOnce);
    sim.decide("a2", Decision::AllowOnce);
    let id = match sim.take(Lane::Decide) {
        Call::Decide { id, .. } => id,
        _ => panic!("not a decision call"),
    };
    assert_eq!(id, "a1");
    // Another request that looks like a2 arrives while its answer waits.
    sim.stream(requested("a2 "));
    assert_eq!(sim.view().undecidable, vec!["a2"]);
    sim.answer(Reply::Decide("a1".into(), Ok(decided("a1", "approved"))));
    assert!(!sim.has(Lane::Decide), "which request would it answer?");
    assert_eq!(sim.view().ignored, 1);
}

#[test]
fn a_card_the_stream_announced_and_the_queue_listed_is_still_answerable() {
    let mut sim = Sim::live();
    sim.stream(requested("a1"));
    sim.stream(reconnected());
    sim.answer_approvals(vec![approval("a1", "pending")]);
    assert!(
        sim.view().undecidable.is_empty(),
        "one request, two sightings"
    );
    sim.decide("a1", Decision::AllowOnce);
    assert!(matches!(sim.take(Lane::Decide), Call::Decide { .. }));
}

#[test]
fn the_words_of_an_abandoned_call_come_back_when_nothing_shows_they_were_sent() {
    let mut sim = Sim::live();
    sim.prompt("tidy"); // the request is lost in a dead socket
    sim.feed(Input::Stop);
    assert!(matches!(sim.take(Lane::Stop), Call::Cancel));
    sim.answer(Reply::Cancel(Ok(cancelled(false))));
    assert_eq!(sim.phase(), Phase::Idle);
    sim.advance(31_000);
    assert_eq!(sim.send_mode(), SendMode::Send);
    let v = sim.view();
    assert_eq!(v.draft, "tidy", "the owner can send it again");
    assert!(v.last_send_uncertain);
    assert!(v.rows.is_empty());
    sim.prompt("tidy");
    assert!(!sim.view().last_send_uncertain, "a new send starts over");
}

#[test]
fn the_words_of_an_abandoned_call_stay_gone_when_the_transcript_shows_them() {
    let mut sim = ended_with_the_call_out();
    sim.advance(31_000);
    let v = sim.view();
    assert_eq!((v.draft.as_str(), v.last_send_uncertain), ("", false));
}

#[test]
fn an_abandoned_calls_reply_before_the_next_send_cannot_end_another_turn() {
    let mut sim = ended_with_the_call_out();
    let old = sim.generation;
    sim.advance(31_000);
    sim.stream(ready(true)); // a turn of another face
    assert_eq!(sim.phase(), Phase::Working);
    sim.answer(Reply::Prompt(old, Some(Ok(finished()))));
    assert_eq!(
        sim.phase(),
        Phase::Working,
        "a retired generation answers nothing"
    );
}

#[test]
fn idle_time_is_counted_only_while_the_model_is_idle() {
    let mut sim = ended_with_the_call_out();
    sim.advance(20_000);
    sim.stream(ready(true)); // busy again: the clock of the idleness starts over
    sim.advance(5_000);
    sim.stream(ready(false));
    sim.advance(20_000);
    assert!(
        sim.replaced.is_empty(),
        "20 s idle, then 20 s: not 30 in a row"
    );
    sim.advance(10_000);
    assert_eq!(sim.replaced, vec![Lane::Prompt]);
}

#[test]
fn a_suspend_long_ago_does_not_shorten_the_bound_of_a_later_send() {
    let mut sim = ended_with_the_call_out();
    sim.suspend(60_000);
    sim.answer_prompt(Ok(finished())); // the call came back after all
    sim.prompt("two");
    sim.entry(&user(3, "two"));
    sim.entry(&answer(4, "again"));
    sim.advance(10_000);
    assert!(sim.replaced.is_empty(), "the bound is 30 s again, not 3");
}

#[test]
fn a_ten_second_gap_between_the_clocks_is_a_suspend() {
    let mut sim = Sim::live();
    sim.suspend(10_000);
    assert!(sim.has(Lane::Status) && sim.has(Lane::Read));
}
