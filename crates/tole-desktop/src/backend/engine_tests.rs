//! The engine's rules for the serve, one look at a time, with no thread and no
//! clock: when it attaches, when it detaches and how long it waits after each.

use std::time::Duration;

use tole_desktop_model::{ServeFailure, ServeState};

use super::engine::{Effect, Input, Out, ATTACH_GAP};
use super::sim::{attach, Sim};

const GAP: u64 = ATTACH_GAP.as_millis() as u64;

#[test]
fn a_serve_that_is_not_ready_gets_no_drivers() {
    let mut sim = Sim::new();
    sim.ready = false;
    for _ in 0..50 {
        assert_eq!(sim.look(), Out::default());
        sim.advance(100);
    }
}

#[test]
fn a_ready_serve_gets_one_attach_under_its_epoch_and_the_engine_waits_for_the_answer() {
    let mut sim = Sim::new();
    sim.epoch = 7;
    assert_eq!(sim.look().effects, [attach(7, &[])]);
    assert_eq!(sim.look(), Out::default(), "the attach is out");
    sim.advance(5_000);
    assert_eq!(sim.look(), Out::default(), "still out, however long");
}

#[test]
fn an_attach_that_worked_is_a_change_and_then_all_is_quiet() {
    let mut sim = Sim::new();
    sim.look();
    let out = sim.feed(Input::Attached(true));
    assert!(out.changed && out.effects.is_empty());
    for _ in 0..20 {
        sim.advance(100);
        assert_eq!(sim.look(), Out::default());
    }
}

#[test]
fn a_serve_that_stops_being_ready_detaches_at_once() {
    let mut sim = Sim::attached();
    sim.ready = false;
    let out = sim.look();
    assert!(out.changed, "the view reads unknown now");
    assert_eq!(out.effects, [Effect::Detach]);
    assert_eq!(sim.look(), Out::default(), "once");
}

#[test]
fn a_detach_does_not_wait_for_the_gap() {
    let mut sim = Sim::attached();
    sim.advance(1);
    sim.ready = false;
    assert_eq!(sim.look().effects, [Effect::Detach]);
}

#[test]
fn a_serve_that_comes_back_gets_new_drivers_after_the_gap_not_before() {
    let mut sim = Sim::attached();
    sim.ready = false;
    sim.look();
    sim.ready = true;
    sim.epoch = 2;
    sim.advance(GAP - 1);
    assert!(sim.look().effects.is_empty(), "too soon");
    sim.advance(1);
    assert_eq!(sim.look().effects, [attach(2, &[])]);
}

#[test]
fn a_replaced_serve_that_looks_ready_both_times_still_gets_new_drivers() {
    // It went away and came back between two looks: only the epoch shows it.
    let mut sim = Sim::attached();
    sim.epoch = 3;
    let out = sim.look();
    assert!(out.changed);
    assert_eq!(out.effects, [Effect::Detach], "the old drivers go first");
    sim.advance(GAP);
    assert_eq!(sim.look().effects, [attach(3, &[])]);
}

#[test]
fn a_failed_attach_is_retried_after_1_2_4_8_16_then_30_seconds() {
    let mut sim = Sim::new();
    assert!(sim.attach_asked());
    for wait in [1_000, 2_000, 4_000, 8_000, 16_000, 30_000, 30_000, 30_000] {
        sim.advance(wait - 1);
        assert!(!sim.attach_asked(), "{wait} ms is not up");
        sim.advance(1);
        assert!(sim.attach_asked(), "{wait} ms is up");
    }
}

#[test]
fn a_failed_attach_is_not_a_change_for_the_view() {
    let mut sim = Sim::new();
    sim.look();
    let out = sim.feed(Input::Attached(false));
    assert_eq!(out, Out::default(), "unknown before, unknown after");
}

/// An engine whose attach failed twice and then worked, at 3 s.
fn after_two_failures() -> Sim {
    let mut sim = Sim::new();
    assert!(sim.attach_asked()); // the next try in 1 s
    sim.advance(1_000);
    assert!(sim.attach_asked()); // the next try in 2 s
    sim.advance(2_000);
    assert_eq!(sim.settle(true), [attach(1, &[])]);
    sim
}

/// The serve is replaced at once: not ready, ready again under a new epoch.
fn replace_serve(sim: &mut Sim) {
    sim.ready = false;
    sim.look();
    sim.ready = true;
    sim.epoch += 1;
    sim.advance(GAP);
}

#[test]
fn the_backoff_goes_on_when_the_drivers_ran_less_than_30_seconds() {
    let mut sim = after_two_failures();
    sim.advance(29_000);
    assert_eq!(sim.look(), Out::default());
    replace_serve(&mut sim);
    assert!(sim.attach_asked(), "after the gap");
    sim.advance(3_999);
    assert!(!sim.attach_asked(), "the third failure waits 4 s");
    sim.advance(1);
    assert!(sim.attach_asked());
}

#[test]
fn the_backoff_starts_over_when_the_drivers_ran_for_30_seconds() {
    let mut sim = after_two_failures();
    sim.advance(30_000);
    assert_eq!(sim.look(), Out::default(), "healthy: the count starts over");
    replace_serve(&mut sim);
    assert!(sim.attach_asked(), "after the gap");
    sim.advance(999);
    assert!(!sim.attach_asked());
    sim.advance(1);
    assert!(sim.attach_asked(), "one second, not four");
}

#[test]
fn a_driver_that_died_is_replaced_after_a_growing_wait() {
    let mut sim = Sim::attached();
    for wait in [1_000, 2_000, 4_000] {
        sim.alive = false;
        let out = sim.look();
        assert!(out.changed);
        assert_eq!(out.effects, [Effect::Detach]);
        sim.alive = true;
        sim.advance(wait - 1);
        assert!(sim.look().effects.is_empty(), "{wait} ms is not up");
        sim.advance(1);
        assert_eq!(sim.settle(true), [attach(1, &[])]);
    }
}

#[test]
fn a_death_while_the_serve_goes_away_is_not_counted_as_a_failure() {
    let mut sim = Sim::attached();
    for _ in 0..5 {
        sim.alive = false;
        sim.ready = false;
        assert_eq!(sim.look().effects, [Effect::Detach]);
        sim.alive = true;
        sim.ready = true;
        sim.epoch += 1;
        sim.advance(GAP);
        assert_eq!(sim.settle(true).len(), 1, "a gap each time, no more");
    }
}

#[test]
fn a_serve_that_flaps_every_tick_gets_new_drivers_once_a_second_at_most() {
    let mut sim = Sim::attached();
    let mut attaches = 0;
    for i in 0..100 {
        sim.ready = i % 2 == 1;
        sim.epoch += 1;
        sim.advance(100);
        let effects = sim.settle(true);
        attaches += (effects.iter())
            .filter(|e| matches!(e, Effect::Attach { .. }))
            .count();
        assert!(
            !sim.ready || !effects.contains(&Effect::Detach),
            "a ready look never detaches the drivers it was ready for"
        );
    }
    // Ten seconds of flapping.
    assert!(attaches <= 11, "{attaches} attaches in 10 s");
    assert!(attaches >= 5, "it does recover: {attaches}");
}

#[test]
fn an_answer_nobody_asked_for_changes_nothing() {
    let mut sim = Sim::new();
    assert_eq!(sim.feed(Input::Attached(true)), Out::default(), "detached");
    let mut sim = Sim::attached();
    assert_eq!(sim.feed(Input::Attached(false)), Out::default(), "attached");
    assert_eq!(sim.look(), Out::default(), "and still attached");
}

#[test]
fn a_ready_serve_after_a_long_outage_is_attached_at_once() {
    let mut sim = Sim::attached();
    sim.ready = false;
    sim.look();
    sim.advance(600_000);
    assert_eq!(sim.look(), Out::default());
    sim.ready = true;
    sim.epoch = 9;
    assert_eq!(sim.look().effects, [attach(9, &[])]);
}

fn failed(detail: &str) -> ServeState {
    ServeState::Failed(ServeFailure::StartFailed {
        detail: detail.into(),
    })
}

fn restarting(attempt: u32) -> ServeState {
    ServeState::Restarting {
        attempt,
        retry_in: Duration::from_secs(u64::from(attempt)),
    }
}

/// Look at a serve in `state`; whether the UI is to be told.
fn told(sim: &mut Sim, state: ServeState) -> bool {
    sim.state = Some(state);
    sim.look().changed
}

#[test]
fn the_first_look_only_records_the_state() {
    let mut sim = Sim::new();
    assert!(
        !told(&mut sim, failed("a")),
        "the UI reads the view at its start"
    );
}

#[test]
fn a_serve_that_goes_through_states_with_no_driver_is_told_at_each_one() {
    // The owner presses Retry on a failed serve: Failed, Starting, Failed.
    let mut sim = Sim::new();
    assert!(!told(&mut sim, failed("a")));
    assert!(told(&mut sim, ServeState::Starting), "Retry: connecting");
    assert!(
        told(&mut sim, failed("b")),
        "failed again: a different reason"
    );
    assert!(!told(&mut sim, failed("b")), "the same state is not news");
    assert!(told(&mut sim, ServeState::Starting));
    assert!(
        told(&mut sim, failed("b")),
        "back to the same failure, still a step"
    );
}

#[test]
fn a_restart_that_goes_on_is_told_at_every_step_after_the_detach() {
    let mut sim = Sim::attached();
    // Running becomes Restarting{1}: the detach, reported once.
    sim.state = Some(restarting(1));
    let out = sim.look();
    assert!(out.changed);
    assert_eq!(out.effects, [Effect::Detach]);
    // No driver is left to report anything: each step is still news.
    assert!(told(&mut sim, restarting(2)), "Restarting{{2}}");
    assert!(!told(&mut sim, restarting(2)));
    sim.advance(5_000);
    let gave_up = ServeState::Failed(ServeFailure::GaveUp {
        attempts: 5,
        detail: "d".into(),
    });
    assert!(
        told(&mut sim, gave_up.clone()),
        "the window can offer Retry"
    );
    assert!(!told(&mut sim, gave_up));
}

#[test]
fn a_new_epoch_under_the_same_state_is_a_change() {
    let mut sim = Sim::new();
    sim.ready = false;
    sim.look();
    sim.epoch += 1;
    assert!(sim.look().changed);
    assert!(!sim.look().changed);
}

#[test]
fn a_state_that_changes_while_attached_and_stays_ready_is_told() {
    // Attached to Running: a version string that changes is a new serve.
    let mut sim = Sim::attached();
    sim.state = Some(ServeState::Attached {
        version: "2".into(),
    });
    sim.epoch = 2;
    let out = sim.look();
    assert!(out.changed);
    assert_eq!(out.effects, [Effect::Detach]);
}
