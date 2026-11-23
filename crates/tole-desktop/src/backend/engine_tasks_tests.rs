//! The engine's rules for the open tasks: what runs equals what is open, the
//! approvals driver follows the same sessions, and a driver that died is
//! replaced after a wait of its own.

use super::engine::{Effect, Input, Out};
use super::sim::{attach, names, Sim};
use crate::driver::task::MAX_OPEN_TASKS;

fn start(id: &str) -> Effect {
    Effect::StartTask(id.to_owned())
}

fn stop(ids: &[&str]) -> Effect {
    Effect::StopTasks(names(ids))
}

fn follow(ids: &[&str]) -> Effect {
    Effect::SetSessions(names(ids))
}

/// A sim that is attached and runs the tasks `ids`.
fn running(ids: &[&str]) -> Sim {
    let mut sim = Sim::attached();
    sim.wanted = names(ids);
    sim.settle(true);
    sim
}

#[test]
fn a_task_opened_while_the_serve_is_down_is_recorded_and_nothing_starts() {
    let mut sim = Sim::new();
    sim.ready = false;
    sim.wanted = names(&["a", "b"]);
    for _ in 0..20 {
        assert_eq!(sim.look(), Out::default());
        sim.advance(100);
    }
}

#[test]
fn the_open_tasks_are_what_the_approvals_driver_starts_with_and_start_with_the_drivers() {
    let mut sim = Sim::new();
    sim.wanted = names(&["a", "b"]);
    let effects = sim.settle(true);
    assert_eq!(
        effects,
        [attach(1, &["a", "b"]), start("a"), start("b")],
        "no look in between"
    );
}

#[test]
fn a_task_opened_while_attached_is_followed_then_started() {
    let mut sim = running(&["a"]);
    sim.wanted = names(&["a", "b"]);
    let out = sim.look();
    assert_eq!(out.effects, [follow(&["a", "b"]), start("b")]);
    assert_eq!(sim.look(), Out::default(), "once");
}

#[test]
fn a_task_closed_is_stopped_and_no_longer_followed() {
    let mut sim = running(&["a", "b"]);
    sim.wanted = names(&["b"]);
    let out = sim.look();
    assert!(out.changed, "the handles the UI reads changed");
    assert_eq!(out.effects, [stop(&["a"]), follow(&["b"])]);
    assert_eq!(sim.look(), Out::default(), "once");
}

#[test]
fn a_close_and_an_open_in_one_look_stops_before_it_starts() {
    let mut sim = running(&["a", "b", "c"]);
    sim.wanted = names(&["b", "c", "d"]);
    let out = sim.look();
    assert_eq!(
        out.effects,
        [stop(&["a"]), follow(&["b", "c", "d"]), start("d")],
        "the serve's stream slots are never over-used"
    );
}

#[test]
fn a_task_that_is_closed_and_opened_again_between_looks_is_never_stopped() {
    let mut sim = running(&["a"]);
    sim.wanted = names(&["a"]);
    assert_eq!(sim.look(), Out::default());
}

#[test]
fn the_open_set_survives_a_restart_of_the_serve() {
    let mut sim = running(&["a", "b", "c"]);
    sim.ready = false;
    let out = sim.look();
    assert_eq!(
        out.effects,
        [Effect::Detach],
        "the drivers go, the wish stays"
    );
    sim.advance(5_000);
    assert_eq!(sim.look(), Out::default(), "nothing while it is down");
    sim.ready = true;
    sim.epoch = 2;
    let effects = sim.settle(true);
    assert_eq!(
        effects,
        [
            attach(2, &["a", "b", "c"]),
            start("a"),
            start("b"),
            start("c")
        ]
    );
}

#[test]
fn a_task_closed_during_the_outage_is_not_started_again() {
    let mut sim = running(&["a", "b"]);
    sim.ready = false;
    sim.look();
    sim.wanted = names(&["b"]);
    sim.ready = true;
    sim.epoch = 2;
    sim.advance(1_000);
    assert_eq!(sim.settle(true), [attach(2, &["b"]), start("b")]);
}

#[test]
fn no_more_than_the_cap_runs_whatever_the_wish_says() {
    let mut sim = Sim::new();
    sim.wanted = names(&["a", "b", "c", "d", "e"]);
    let effects = sim.settle(true);
    let starts = effects.iter().filter(|e| matches!(e, Effect::StartTask(_)));
    assert_eq!(starts.count(), MAX_OPEN_TASKS);
    assert_eq!(effects[0], attach(1, &["a", "b", "c"]));
}

#[test]
fn a_task_that_could_not_be_started_is_tried_again_after_a_growing_wait() {
    let mut sim = running(&[]);
    sim.wanted = names(&["a"]);
    assert_eq!(sim.settle_with(true, false), [follow(&["a"]), start("a")]);
    for wait in [1_000, 2_000, 4_000] {
        sim.advance(wait - 1);
        assert_eq!(sim.look(), Out::default(), "{wait} ms is not up");
        sim.advance(1);
        assert_eq!(
            sim.settle_with(true, false),
            [start("a")],
            "{wait} ms is up"
        );
    }
}

#[test]
fn a_task_driver_that_died_is_stopped_and_started_again_after_a_wait() {
    let mut sim = running(&["a", "b"]);
    sim.dead = names(&["a"]);
    let out = sim.look();
    assert!(out.changed);
    assert_eq!(out.effects, [stop(&["a"])], "no start before the wait");
    sim.dead.clear();
    sim.advance(999);
    assert_eq!(sim.look(), Out::default());
    sim.advance(1);
    assert_eq!(sim.settle(true), [start("a")]);
    // The next death waits twice as long.
    sim.dead = names(&["a"]);
    sim.look();
    sim.dead.clear();
    sim.advance(1_999);
    assert_eq!(sim.look(), Out::default());
    sim.advance(1);
    assert_eq!(sim.settle(true), [start("a")]);
}

#[test]
fn a_task_driver_that_died_and_was_closed_is_stopped_once() {
    let mut sim = running(&["a"]);
    sim.dead = names(&["a"]);
    sim.wanted = Vec::new();
    let out = sim.look();
    assert_eq!(out.effects, [stop(&["a"]), follow(&[])]);
}

#[test]
fn a_death_the_engine_did_not_start_is_ignored() {
    let mut sim = running(&["a"]);
    sim.dead = names(&["zz"]);
    assert_eq!(sim.look(), Out::default());
}

#[test]
fn the_wait_after_a_death_starts_over_when_no_driver_died_for_30_seconds() {
    let mut sim = running(&["a"]);
    for _ in 0..2 {
        sim.dead = names(&["a"]);
        sim.look();
        sim.dead.clear();
        sim.advance(8_000);
        sim.settle(true);
    }
    sim.advance(30_000);
    sim.look();
    sim.dead = names(&["a"]);
    sim.look();
    sim.dead.clear();
    sim.advance(999);
    assert_eq!(sim.look(), Out::default());
    sim.advance(1);
    assert_eq!(sim.settle(true), [start("a")], "1 s, not 4 s");
}

#[test]
fn a_task_driver_that_died_with_the_approvals_driver_is_one_detach() {
    let mut sim = running(&["a"]);
    sim.alive = false;
    sim.dead = names(&["a"]);
    let out = sim.look();
    assert_eq!(out.effects, [Effect::Detach]);
    sim.alive = true;
    sim.dead.clear();
    sim.advance(1_000);
    assert_eq!(sim.settle(true), [attach(1, &["a"]), start("a")]);
}

#[test]
fn a_late_task_answer_after_a_detach_changes_nothing() {
    let mut sim = running(&["a"]);
    sim.ready = false;
    sim.look();
    let out = sim.feed(Input::TaskStarted("a".into(), false));
    assert_eq!(out, Out::default());
}

#[test]
fn a_task_that_keeps_dying_does_not_delay_a_task_opened_later() {
    let mut sim = running(&["a"]);
    for round in 0..4_u64 {
        sim.dead = names(&["a"]);
        sim.look();
        sim.dead.clear();
        // However long a's wait has grown, b starts at once, and only once.
        if round == 0 {
            sim.wanted = names(&["a", "b"]);
            assert_eq!(sim.settle(true), [follow(&["a", "b"]), start("b")]);
        }
        sim.advance(1_000 << round);
        assert_eq!(
            sim.settle(true),
            [start("a")],
            "a's own wait: round {round}"
        );
    }
}

#[test]
fn three_deaths_in_one_look_each_wait_their_own_first_failure() {
    let mut sim = running(&["a", "b", "c"]);
    sim.dead = names(&["a", "b", "c"]);
    let out = sim.look();
    assert_eq!(out.effects, [stop(&["a", "b", "c"])]);
    sim.dead.clear();
    sim.advance(999);
    assert_eq!(sim.look(), Out::default(), "1 s, not 4 s, and not before");
    sim.advance(1);
    assert_eq!(sim.settle(true), [start("a"), start("b"), start("c")]);
    // The second death of each waits 2 s, each on its own count.
    sim.dead = names(&["a", "b", "c"]);
    sim.look();
    sim.dead.clear();
    sim.advance(1_999);
    assert_eq!(sim.look(), Out::default());
    sim.advance(1);
    assert_eq!(sim.settle(true), [start("a"), start("b"), start("c")]);
}

#[test]
fn the_deaths_of_one_task_do_not_make_another_tasks_first_wait_longer() {
    let mut sim = running(&["a", "b"]);
    for _ in 0..3 {
        sim.dead = names(&["a"]);
        sim.look();
        sim.dead.clear();
        sim.advance(30_000);
        sim.settle(true);
    }
    // a has failed three times; b's first death still waits 1 s.
    sim.dead = names(&["b"]);
    sim.look();
    sim.dead.clear();
    sim.advance(999);
    assert_eq!(sim.look(), Out::default());
    sim.advance(1);
    assert_eq!(sim.settle(true), [start("b")]);
}

#[test]
fn a_task_that_could_not_start_waits_alone() {
    let mut sim = running(&[]);
    sim.wanted = names(&["a", "b"]);
    let out = sim.look();
    assert_eq!(out.effects, [follow(&["a", "b"]), start("a"), start("b")]);
    sim.feed(Input::TaskStarted("a".into(), false));
    sim.feed(Input::TaskStarted("b".into(), true));
    sim.advance(999);
    assert_eq!(sim.look(), Out::default());
    sim.advance(1);
    assert_eq!(sim.settle(true), [start("a")], "only a, after its 1 s");
}

#[test]
fn a_closed_task_is_forgotten_with_its_failures() {
    let mut sim = running(&["a"]);
    // Three deaths: the waits were 1 s, 2 s and now 4 s.
    for wait in [1_000, 2_000] {
        sim.dead = names(&["a"]);
        sim.look();
        sim.dead.clear();
        sim.advance(wait);
        sim.settle(true);
    }
    sim.dead = names(&["a"]);
    sim.look();
    sim.dead.clear();
    // The app closes it, then opens it again: no wait is left to serve.
    sim.wanted = Vec::new();
    assert_eq!(sim.look().effects, [follow(&[])]);
    sim.wanted = names(&["a"]);
    assert_eq!(sim.settle(true), [follow(&["a"]), start("a")], "at once");
    // And its next death is a first failure again: 1 s, not 8 s.
    sim.dead = names(&["a"]);
    sim.look();
    sim.dead.clear();
    sim.advance(999);
    assert_eq!(sim.look(), Out::default());
    sim.advance(1);
    assert_eq!(sim.settle(true), [start("a")]);
}
