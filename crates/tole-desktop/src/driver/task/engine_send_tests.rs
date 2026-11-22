//! Prompt, steer note and what a failed call means.

use std::io;

use tole_client::Error;
use tole_desktop_model::task::{Phase, SendMode};

use super::calls::{Call, Lane, Reply};
use super::engine::Input;
use super::fixtures::*;
use super::sim::Sim;

fn io_error() -> Error {
    Error::Io(io::Error::other("reset"))
}

/// A prompt that is out and whose user entry the stream has delivered.
fn running(sim: &mut Sim) {
    sim.prompt("tidy");
    sim.entry(&user(1, "tidy"));
}

#[test]
fn a_prompt_goes_out_once_and_a_second_waits_until_the_call_returned() {
    let mut sim = Sim::live();
    assert_eq!(sim.prompt("tidy"), "tidy");
    assert_eq!(sim.phase(), Phase::Working);
    sim.entry(&user(1, "tidy"));
    // The answer ends the turn before the call returns: sending again now
    // would let the first reply's end of turn land on the second turn.
    sim.entry(&answer(2, "done"));
    assert_eq!(sim.phase(), Phase::Idle);
    assert_eq!(sim.send_mode(), SendMode::Disabled(Phase::Working));
    sim.feed(Input::Send("again".into()));
    assert!(!sim.has(Lane::Prompt), "refused while the call is out");
    sim.answer_prompt(Ok(finished()));
    assert_eq!(sim.send_mode(), SendMode::Send);
    assert_eq!(sim.prompt("again"), "again");
}

#[test]
fn nothing_is_sent_while_loading_for_blank_text_or_in_the_wrong_mode() {
    let mut loading = Sim::new(false);
    loading.feed(Input::Send("early".into()));
    assert!(!loading.has(Lane::Prompt));
    let mut sim = Sim::live();
    sim.feed(Input::Send("  \n".into()));
    sim.feed(Input::Steer("note".into()));
    assert!(
        sim.none_pending(),
        "blank, and a steer with no turn running"
    );
    sim.prompt("tidy");
    sim.feed(Input::Send("second".into()));
    assert!(sim.none_pending(), "a prompt while a turn runs");
}

#[test]
fn a_finished_turn_ends_on_the_reply() {
    let mut sim = Sim::live();
    sim.prompt("tidy");
    sim.answer_prompt(Ok(finished()));
    assert_eq!(sim.phase(), Phase::Idle);
    assert_eq!(sim.send_mode(), SendMode::Send);
}

#[test]
fn a_409_keeps_the_turn_busy_and_a_poll_ends_it_when_the_server_is_idle() {
    let mut sim = Sim::live();
    sim.prompt("tidy");
    sim.answer_prompt(Err(status_code(409)));
    assert_eq!(sim.phase(), Phase::Working, "another face's turn");
    assert_eq!(sim.view().draft, "tidy", "the words come back");
    assert!(!sim.has(Lane::Status));
    sim.advance(500);
    sim.status_answer(true);
    assert_eq!(sim.phase(), Phase::Working);
    sim.advance(500);
    sim.status_answer(false);
    assert_eq!(sim.phase(), Phase::Idle);
}

#[test]
fn a_dropped_connection_gives_the_words_back_and_asks_whether_the_turn_runs() {
    let mut sim = Sim::live();
    sim.prompt("tidy");
    sim.answer_prompt(Err(io_error()));
    assert_eq!(
        (sim.phase(), sim.view().draft),
        (Phase::Idle, "tidy".into())
    );
    sim.status_answer(true);
    assert_eq!(sim.phase(), Phase::Working, "the turn runs after all");
    sim.advance(500);
    sim.status_answer(false);
    assert_eq!(sim.phase(), Phase::Idle);
}

#[test]
fn a_status_check_that_finds_nothing_running_asks_no_more() {
    let mut sim = Sim::live();
    sim.prompt("tidy");
    sim.answer_prompt(Err(io_error()));
    sim.status_answer(false);
    sim.advance(60_000);
    assert!(sim.none_pending());
}

#[test]
fn a_status_check_that_fails_is_retried_a_few_times_and_then_dropped() {
    let mut sim = Sim::live();
    sim.prompt("tidy");
    sim.answer_prompt(Err(io_error()));
    for _ in 0..5 {
        match sim.take(Lane::Status) {
            Call::Status { token, check } => sim.answer(Reply::Status {
                token,
                check,
                reply: Err(io_error()),
            }),
            _ => panic!("not a status call"),
        }
        sim.advance(60_000);
    }
    assert!(
        !sim.has(Lane::Status),
        "five tries, then the stream is left to say"
    );
}

#[test]
fn a_status_check_of_a_failed_send_cannot_end_the_next_prompts_turn() {
    let mut sim = Sim::live();
    sim.prompt("tidy");
    sim.answer_prompt(Err(io_error()));
    let (token, check) = match sim.take(Lane::Status) {
        Call::Status { token, check } => (token, check),
        _ => panic!("not a status call"),
    };
    // The owner sends again before the check answers; the serve took it.
    sim.prompt("again");
    sim.entry(&user(1, "again"));
    sim.answer(Reply::Status {
        token,
        check,
        reply: Ok(status(false)),
    });
    assert_eq!(
        sim.phase(),
        Phase::Working,
        "the check was about the first send"
    );
}

#[test]
fn a_bare_approval_required_is_settled_by_a_status_read_not_by_ending_the_turn() {
    let mut sim = Sim::live();
    running(&mut sim);
    sim.answer_prompt(Ok(bare_approval_required()));
    assert_eq!(
        sim.phase(),
        Phase::Working,
        "no TurnEnded: a write may be running"
    );
    sim.advance(500);
    sim.status_answer(false);
    assert_eq!(sim.phase(), Phase::Idle);
}

#[test]
fn a_bare_reply_whose_user_entry_never_came_is_still_settled_by_the_status() {
    let mut sim = Sim::live();
    sim.prompt("tidy"); // its user entry has not been seen: the stream is down
    sim.answer_prompt(Ok(bare_approval_required()));
    assert_eq!(sim.phase(), Phase::Working);
    sim.advance(500);
    // The call returned: it is no longer going out, so a not-busy answer is
    // final, not ignored for ever.
    sim.status_answer(false);
    assert_eq!(sim.phase(), Phase::Idle);
    sim.advance(120_000);
    assert!(sim.none_pending());
}

#[test]
fn a_reply_with_an_approval_row_shows_the_card() {
    let mut sim = Sim::live();
    running(&mut sim);
    sim.answer_prompt(Ok(asking("a1")));
    assert_eq!(sim.phase(), Phase::WaitingApproval);
    assert_eq!(sim.view().approvals.len(), 1);
}

#[test]
fn a_late_reply_does_not_end_an_allowed_turn_that_is_running() {
    let mut sim = Sim::live();
    running(&mut sim);
    sim.stream(requested("a1"));
    sim.stream(resolved("a1", "approved"));
    assert_eq!(sim.phase(), Phase::Resuming);
    // The reply of the part of the turn that asked comes after the allow.
    sim.answer_prompt(Ok(asking("a1")));
    assert_eq!(
        sim.phase(),
        Phase::Resuming,
        "its TurnEnded is not this turn's"
    );
    sim.entry(&answer(2, "done"));
    assert_eq!(sim.phase(), Phase::Idle);
}

#[test]
fn a_steer_note_goes_while_the_turn_runs_and_failures_give_it_back() {
    let mut sim = Sim::live();
    running(&mut sim);
    sim.feed(Input::Steer("also x".into()));
    match sim.take(Lane::Steer) {
        Call::Interrupt(req) => assert_eq!(req.text, "also x"),
        _ => panic!("not an interrupt"),
    }
    assert_eq!(sim.send_mode(), SendMode::Disabled(Phase::Working));
    sim.feed(Input::Steer("and y".into()));
    assert!(!sim.has(Lane::Steer), "one note at a time");
    sim.answer(Reply::Interrupt(Ok(steered())));
    assert_eq!(
        (sim.send_mode(), sim.view().draft),
        (SendMode::Steer, "".into())
    );
    sim.feed(Input::Steer("and y".into()));
    sim.take(Lane::Steer);
    sim.answer(Reply::Interrupt(Err(io_error())));
    assert_eq!(sim.view().draft, "and y");
    assert!(sim.none_pending());
}

#[test]
fn a_409_on_a_steer_note_checks_whether_the_turn_still_runs() {
    let mut sim = Sim::live();
    running(&mut sim);
    sim.feed(Input::Steer("x".into()));
    sim.take(Lane::Steer);
    sim.answer(Reply::Interrupt(Err(status_code(409))));
    sim.status_answer(false);
    assert_eq!(
        sim.phase(),
        Phase::Idle,
        "the model was wrong that a turn ran"
    );
}
