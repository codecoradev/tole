//! After an allow, a busy snapshot or a lost link: the model is never left
//! busy with nothing watching, and an old read never settles a new turn.

use std::io;

use tole_client::Error;
use tole_desktop_model::task::Phase;

use super::calls::{Call, Lane, Reply};
use super::fixtures::*;
use super::sim::Sim;

/// A task that asked for an OK and was allowed on the stream.
fn allowed() -> Sim {
    let mut sim = Sim::live();
    sim.prompt("tidy");
    sim.entry(&user(1, "tidy"));
    sim.stream(requested("a1"));
    sim.stream(resolved("a1", "approved"));
    sim.answer_prompt(Ok(asking("a1")));
    assert_eq!(sim.phase(), Phase::Resuming);
    sim
}

#[test]
fn a_not_busy_answer_right_after_an_allow_is_not_the_end() {
    let mut sim = allowed();
    sim.advance(499);
    assert!(
        !sim.has(Lane::Status),
        "the first poll is 500 ms after the allow"
    );
    sim.advance(1);
    sim.status_answer(false);
    assert_eq!(
        sim.phase(),
        Phase::Resuming,
        "serve answers 202 before the turn starts"
    );
    sim.advance(500);
    sim.status_answer(true);
    assert_eq!(sim.phase(), Phase::Working, "a busy answer confirms");
    sim.advance(2_999);
    assert!(
        !sim.has(Lane::Status),
        "a turn known to run is polled slowly"
    );
    sim.advance(1);
    sim.status_answer(false);
    assert_eq!(sim.phase(), Phase::Idle);
    sim.advance(60_000);
    assert!(sim.none_pending(), "the watch is over");
}

#[test]
fn an_allowed_turn_with_no_sign_of_life_ends_after_five_seconds_though_polls_hang() {
    let mut sim = allowed();
    sim.advance(500);
    sim.take(Lane::Status); // never answered: the call hangs
    sim.advance(4_499);
    assert_eq!(sim.phase(), Phase::Resuming);
    sim.advance(1);
    assert_eq!(
        sim.phase(),
        Phase::Idle,
        "the timeout runs on time, not on answers"
    );
}

#[test]
fn a_poll_that_fails_twice_with_no_sign_of_life_ends_the_turn() {
    let mut sim = allowed();
    for wait in [500, 1_000] {
        sim.advance(wait);
        match sim.take(Lane::Status) {
            Call::Status { token, check } => sim.answer(Reply::Status {
                token,
                check,
                reply: Err(Error::Io(io::Error::other("reset"))),
            }),
            _ => panic!("not a status call"),
        }
    }
    assert_eq!(sim.phase(), Phase::Idle);
}

#[test]
fn the_streams_signs_of_life_keep_an_allowed_turn_open_past_the_timeout() {
    let mut sim = allowed();
    sim.stream(delta("working"));
    assert_eq!(sim.phase(), Phase::Working);
    sim.advance(500);
    sim.take(Lane::Status);
    sim.advance(10_000);
    assert_eq!(sim.phase(), Phase::Working, "alive: no timeout");
}

#[test]
fn a_status_read_from_before_the_turn_ended_cannot_make_the_model_busy_again() {
    let mut sim = Sim::live();
    sim.stream(ready(true));
    assert_eq!(sim.phase(), Phase::Working);
    sim.advance(500);
    let old = match sim.take(Lane::Status) {
        Call::Status { token, check } => (token, check),
        _ => panic!("not a status call"),
    };
    sim.entry(&answer(1, "done"));
    assert_eq!(sim.phase(), Phase::Idle);
    sim.answer(Reply::Status {
        token: old.0,
        check: old.1,
        reply: Ok(status(true)),
    });
    assert_eq!(sim.phase(), Phase::Idle, "a stale answer is dropped");
    sim.advance(60_000);
    assert!(sim.none_pending(), "and nothing is polled for an idle task");
}

#[test]
fn a_status_read_from_an_earlier_watch_cannot_end_the_next_turn() {
    let mut sim = Sim::live();
    sim.stream(ready(true));
    sim.advance(500);
    let old = match sim.take(Lane::Status) {
        Call::Status { token, check } => (token, check),
        _ => panic!("not a status call"),
    };
    sim.entry(&answer(1, "done"));
    assert_eq!(sim.phase(), Phase::Idle);
    // A new turn starts and is watched by a new watcher.
    sim.stream(ready(true));
    assert_eq!(sim.phase(), Phase::Working);
    sim.answer(Reply::Status {
        token: old.0,
        check: old.1,
        reply: Ok(status(false)),
    });
    assert_eq!(
        sim.phase(),
        Phase::Working,
        "the old read says nothing of this turn"
    );
}

#[test]
fn a_status_read_from_before_an_allow_cannot_settle_the_turn_the_allow_started() {
    let mut sim = Sim::live();
    sim.stream(ready(true));
    sim.advance(500);
    let old = match sim.take(Lane::Status) {
        Call::Status { token, check } => (token, check),
        _ => panic!("not a status call"),
    };
    sim.stream(requested("a1"));
    sim.stream(resolved("a1", "approved"));
    assert_eq!(sim.phase(), Phase::Resuming);
    // The old read says the turn is over: it came before the allow.
    sim.answer(Reply::Status {
        token: old.0,
        check: old.1,
        reply: Ok(status(false)),
    });
    assert_eq!(sim.phase(), Phase::Resuming);
}

#[test]
fn a_busy_snapshot_from_the_stream_is_watched_until_the_server_is_idle() {
    let mut sim = Sim::live();
    sim.stream(ready(true));
    sim.advance(500);
    sim.status_answer(true);
    assert_eq!(sim.phase(), Phase::Working);
    sim.advance(500);
    sim.status_answer(false);
    assert_eq!(sim.phase(), Phase::Idle);
}

#[test]
fn polling_that_cannot_reach_the_serve_ends_in_a_lost_link_not_a_stuck_turn() {
    let mut sim = Sim::live();
    sim.stream(ready(true));
    let mut waited = 0;
    while sim.phase() != Phase::Offline {
        sim.advance(500);
        if let Some(Call::Status { token, check }) =
            sim.has(Lane::Status).then(|| sim.take(Lane::Status))
        {
            waited += 1;
            sim.answer(Reply::Status {
                token,
                check,
                reply: Err(Error::Io(io::Error::other("reset"))),
            });
        }
        sim.advance(4_500);
        assert!(waited < 40, "it ends");
    }
    assert!(
        waited >= 17,
        "five fast failures, twelve slow ones: {waited}"
    );
}

#[test]
fn a_suspend_reads_the_status_and_the_queue_again() {
    let mut sim = Sim::live();
    sim.suspend(60_000);
    assert!(sim.has(Lane::Status) && sim.has(Lane::Read));
    sim.status_answer(true);
    assert_eq!(
        sim.phase(),
        Phase::Working,
        "the turn ran while the machine slept"
    );
    sim.answer_approvals(vec![approval("a1", "pending")]);
    assert_eq!(sim.view().approvals.len(), 1);
}

#[test]
fn nothing_is_polled_for_an_idle_task() {
    let mut sim = Sim::live();
    sim.advance(120_000);
    assert!(sim.none_pending());
}
