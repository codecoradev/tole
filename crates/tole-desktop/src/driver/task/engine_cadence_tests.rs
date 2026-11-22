//! How often the driver asks: the REST reads while no stream is up, the poll
//! of a resumed turn, and what a repaint is told.

use std::io;

use tole_client::Error;
use tole_desktop_model::approvals::Decision;
use tole_desktop_model::task::Phase;

use super::calls::{Call, Lane, Reply};
use super::engine::Input;
use super::fixtures::*;
use super::sim::Sim;

fn io_error() -> Error {
    Error::Io(io::Error::other("reset"))
}

/// A task allowed on the stream, and then shown alive by its text.
fn resumed_and_alive() -> Sim {
    let mut sim = Sim::live();
    sim.prompt("tidy");
    sim.entry(&user(1, "tidy"));
    sim.stream(requested("a1"));
    sim.stream(resolved("a1", "approved"));
    sim.answer_prompt(Ok(asking("a1")));
    sim.stream(delta("working"));
    assert_eq!(sim.phase(), Phase::Working);
    sim
}

/// Answer the status call that is due, if there is one; whether there was.
fn answer_status(sim: &mut Sim, reply: Result<tole_client::SessionStatus, Error>) -> bool {
    if !sim.has(Lane::Status) {
        return false;
    }
    match sim.take(Lane::Status) {
        Call::Status { token, check } => sim.answer(Reply::Status {
            token,
            check,
            reply,
        }),
        _ => panic!("not a status call"),
    }
    true
}

#[test]
fn while_no_stream_is_up_the_status_and_the_queue_are_read_over_rest_at_a_slow_pace() {
    let mut sim = Sim::new(true); // the stream never opens
    sim.advance(4_999);
    assert!(
        sim.none_pending(),
        "a stream that opens at once costs nothing"
    );
    sim.advance(1);
    assert!(sim.has(Lane::Status) && sim.has(Lane::Read));
    sim.status_answer(false);
    sim.answer_approvals(vec![approval("a1", "pending")]);
    let v = sim.view();
    assert_eq!(v.status.phase, Phase::WaitingApproval, "the card is there");
    assert_eq!(v.approvals.len(), 1);
    for gap in [10_000, 20_000, 40_000, 60_000, 60_000] {
        sim.advance(gap - 1);
        assert!(sim.none_pending(), "not before {gap} ms");
        sim.advance(1);
        assert!(sim.has(Lane::Status) && sim.has(Lane::Read), "at {gap} ms");
        sim.status_answer(false);
        sim.answer_approvals(vec![approval("a1", "pending")]);
    }
}

#[test]
fn stop_works_without_a_stream_because_the_status_says_a_turn_runs() {
    let mut sim = Sim::new(true);
    sim.advance(5_000);
    sim.status_answer(true);
    assert_eq!(sim.phase(), Phase::Working);
    assert!(sim.view().status.can_stop);
}

#[test]
fn a_stream_that_opens_ends_the_polling_and_a_lost_one_starts_it_again() {
    let mut sim = Sim::new(true);
    sim.advance(5_000);
    sim.status_answer(false);
    sim.answer_approvals(vec![]);
    sim.up(false);
    sim.answer_approvals(vec![]);
    sim.advance(600_000);
    assert!(sim.none_pending(), "the stream carries it now");
    sim.stream_error(io_error());
    sim.advance(4_999);
    assert!(sim.none_pending());
    sim.advance(1);
    assert!(sim.has(Lane::Status) && sim.has(Lane::Read));
}

#[test]
fn a_closed_session_is_not_read() {
    let mut sim = Sim::new(true);
    sim.stream(frame("closed", serde_json::json!({"id": S})));
    sim.advance(600_000);
    assert!(sim.none_pending());
}

#[test]
fn a_resumed_turn_whose_polls_all_fail_is_polled_a_few_hundred_times_an_hour_not_thousands() {
    let mut sim = resumed_and_alive();
    let (mut polls, mut quiet_from) = (0, None);
    for _ in 0..(3_600_000 / 250) {
        sim.advance(250);
        if answer_status(&mut sim, Err(io_error())) {
            polls += 1;
            quiet_from.get_or_insert(sim.changes);
        }
    }
    assert!((600..=800).contains(&polls), "{polls} polls in an hour");
    assert_eq!(sim.phase(), Phase::Working, "failed polls end nothing");
    assert_eq!(Some(sim.changes), quiet_from, "and repaint nothing");
}

#[test]
fn a_resumed_turn_that_runs_is_polled_slowly_and_a_poll_that_says_nothing_new_repaints_nothing() {
    let mut sim = resumed_and_alive();
    sim.advance(3_000);
    answer_status(&mut sim, Ok(status(true)));
    let (mut polls, changes) = (0, sim.changes);
    for _ in 0..(1_800_000 / 250) {
        sim.advance(250);
        if answer_status(&mut sim, Ok(status(true))) {
            polls += 1;
        }
    }
    assert!(
        (550..=600).contains(&polls),
        "{polls} polls in half an hour"
    );
    assert_eq!(sim.changes, changes);
}

#[test]
fn commands_the_driver_refuses_are_counted_for_the_view() {
    let mut loading = Sim::new(false);
    loading.feed(Input::Send("early".into()));
    assert_eq!(loading.view().ignored, 1, "the history is still being read");
    let mut sim = Sim::live();
    sim.feed(Input::Steer("note".into()));
    sim.feed(Input::Stop);
    sim.decide("nobody", Decision::AllowOnce);
    sim.feed(Input::Send(" ".into()));
    assert_eq!(sim.view().ignored, 4);
    sim.prompt("tidy");
    assert_eq!(
        sim.view().ignored,
        4,
        "a command that went out is not counted"
    );
}
