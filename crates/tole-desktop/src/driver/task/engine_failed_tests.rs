//! A call that failed after it may have reached the serve must not put words
//! back in the draft blindly, and a height that did not move is no change.

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

fn refused() -> Error {
    Error::Status {
        status: 400,
        message: "bad request".into(),
        body: serde_json::Value::Null,
    }
}

#[test]
fn a_failed_prompt_that_the_transcript_shows_is_not_given_back() {
    let mut sim = Sim::live();
    sim.prompt("tidy");
    // The serve took it and the stream shows it; then the connection resets.
    sim.entry(&user(1, "tidy"));
    sim.answer_prompt(Err(io_error()));
    let v = sim.view();
    assert_eq!((v.draft.as_str(), v.last_send_uncertain), ("", false));
}

#[test]
fn a_failed_prompt_nothing_shows_comes_back_with_the_note() {
    let mut sim = Sim::live();
    sim.prompt("tidy");
    sim.answer_prompt(Err(io_error()));
    let v = sim.view();
    assert_eq!(v.draft, "tidy");
    assert!(v.last_send_uncertain, "it may have been sent");
}

#[test]
fn a_prompt_the_serve_refused_comes_back_with_no_doubt() {
    let mut sim = Sim::live();
    sim.prompt("tidy");
    sim.answer_prompt(Err(refused()));
    let v = sim.view();
    assert_eq!((v.draft.as_str(), v.last_send_uncertain), ("tidy", false));
}

#[test]
fn a_failed_steer_note_comes_back_with_the_note_only_when_it_may_have_arrived() {
    let mut sim = Sim::live();
    sim.prompt("tidy");
    sim.entry(&user(1, "tidy"));
    sim.feed(Input::Steer("also x".into()));
    assert!(matches!(sim.take(Lane::Steer), Call::Interrupt(_)));
    sim.answer(Reply::Interrupt(Err(io_error())));
    let v = sim.view();
    assert_eq!((v.draft.as_str(), v.last_send_uncertain), ("also x", true));
    assert_eq!(sim.send_mode(), SendMode::Steer);

    let mut sim = Sim::live();
    sim.prompt("tidy");
    sim.entry(&user(1, "tidy"));
    sim.feed(Input::Steer("also y".into()));
    sim.take(Lane::Steer);
    sim.answer(Reply::Interrupt(Err(refused())));
    let v = sim.view();
    assert_eq!((v.draft.as_str(), v.last_send_uncertain), ("also y", false));
}

#[test]
fn measuring_the_same_rows_again_is_no_change() {
    let mut sim = Sim::live();
    sim.prompt("tidy");
    sim.entry(&user(1, "tidy"));
    assert_eq!(sim.phase(), Phase::Working);
    sim.feed(Input::Heights(vec![(0, 55.0)]));
    let after_first = sim.changes;
    sim.feed(Input::Heights(vec![(0, 55.0)]));
    assert_eq!(sim.changes, after_first, "the same height repaints nothing");
    sim.feed(Input::Heights(vec![(0, 60.0)]));
    assert_eq!(sim.changes, after_first + 1, "a moved height does");
    sim.feed(Input::Heights(vec![(99, 60.0)]));
    assert_eq!(sim.changes, after_first + 1, "a row out of range does not");
}
