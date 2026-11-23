//! The rules of the list: when it is asked for, what an answer does and what a
//! loss of knowledge does. No thread, no clock but the one the test moves.

use tole_client::CronJobs;
use tole_desktop_model::{Load, Scheduler, SchedulerHost};

use super::engine::{Input, STALE};
use super::fixtures::*;

#[test]
fn the_list_is_asked_for_at_once_and_the_first_answer_makes_it_known() {
    let mut t = T::new();
    let out = t.start();
    let call = load_of(&out).expect("at once");
    assert!(runs_call_of(&out).is_none(), "nothing is selected");
    let v = t.view();
    assert_eq!(v.page.load, Load::Loading);
    assert!(v.jobs_feed.loading && v.page.rows.is_empty());
    assert_eq!(v.counts(), None, "no number before an answer");

    let out = t.step(Input::Jobs(call, jobs_reply(three())));
    assert!(out.changed && out.effects.is_empty());
    let v = t.view();
    assert_eq!((v.page.load.clone(), v.page.rows.len()), (Load::Ready, 3));
    assert_eq!(v.counts(), Some((1, 2)));
    assert_eq!(v.page.scheduler, Scheduler::Running(SchedulerHost::Serve));
    assert!(!v.jobs_feed.loading && v.jobs_feed.failures == 0);
    assert!(
        load_of(&t.after(10_000)).is_none(),
        "one call, not a stream"
    );
}

#[test]
fn the_list_is_read_again_after_the_refresh_interval_and_not_before() {
    let mut t = T::known();
    let (_, waited) = t.next_load(60_000).expect("a refresh");
    assert_eq!(waited, 30_000);
    assert_eq!(
        t.page().load,
        Load::Ready,
        "a refresh in flight keeps the rows"
    );
    assert!(t.view().jobs_feed.loading);
}

#[test]
fn an_owner_refresh_skips_the_wait_but_not_the_pace_of_one_call_a_second() {
    let mut t = T::known();
    t.after(500);
    let out = t.step(Input::Refresh);
    assert!(
        load_of(&out).is_none(),
        "the last call began half a second ago"
    );
    let out = t.after(500);
    assert!(load_of(&out).is_some());
    let more = t.step(Input::Refresh);
    assert!(
        load_of(&more).is_none(),
        "one call is out: it does not count"
    );
}

#[test]
fn a_failed_first_load_retries_with_a_growing_wait_and_never_gives_up() {
    let mut t = T::new();
    let mut call = load_of(&t.start()).expect("first");
    let mut waits = Vec::new();
    for _ in 0..8 {
        t.step(Input::Jobs(call, Err(io_error())));
        let v = t.view();
        assert!(matches!(v.page.load, Load::Failed(_)) && v.page.rows.is_empty());
        assert_eq!(v.page.scheduler, Scheduler::NotRunning);
        let (next, waited) = t.next_load(60_000).expect("a retry");
        waits.push(waited / 1_000);
        call = next;
    }
    assert_eq!(waits, [1, 2, 4, 8, 16, 30, 30, 30]);
    t.step(Input::Jobs(call, jobs_reply(three())));
    let v = t.view();
    assert_eq!((v.page.load, v.jobs_feed.failures), (Load::Ready, 0));
    let (_, waited) = t.next_load(60_000).expect("refresh");
    assert_eq!(waited, 30_000, "the backoff started over");
}

#[test]
fn a_failed_refresh_keeps_the_rows_marked_failed_and_never_a_count() {
    let mut t = T::known();
    let (call, _) = t.next_load(40_000).expect("refresh");
    t.step(Input::Jobs(call, Err(status_error(503, "busy"))));
    let v = t.view();
    assert_eq!(v.page.rows.len(), 3, "the rows stay");
    assert!(matches!(&v.page.load, Load::Failed(m) if m.contains("503")));
    assert_eq!((v.counts(), v.jobs_feed.failures), (None, 1));
    assert_eq!(v.page.scheduler, Scheduler::NotRunning);
    let (call, _) = t.next_load(5_000).expect("retry");
    t.step(Input::Jobs(call, jobs_reply(three())));
    assert_eq!(t.view().counts(), Some((1, 2)));
}

#[test]
fn rows_older_than_twice_the_refresh_are_marked_and_a_late_answer_heals_them() {
    let mut t = T::known();
    let (hung, _) = t.next_load(31_000).expect("a refresh that never answers");
    t.after(31_500);
    let v = t.view();
    assert_eq!(v.page.load, Load::Failed(STALE.into()));
    assert_eq!((v.page.rows.len(), v.counts()), (3, None));
    assert!(load_of(&t.after(1_000)).is_none(), "its slot is held");
    t.step(Input::Jobs(hung, jobs_reply(three())));
    assert_eq!(t.view().counts(), Some((1, 2)));
}

#[test]
fn a_suspend_marks_the_rows_and_the_call_that_was_out_is_ignored() {
    let mut t = T::known();
    let (out_before, _) = t.next_load(31_000).expect("refresh");
    let out = t.jump(3_600_000);
    assert_eq!(t.page().load, Load::Failed(STALE.into()));
    assert!(load_of(&out).is_none(), "the old call still holds the slot");
    // Its answer describes the time before the sleep: it frees the slot and
    // shows nothing, and a new call follows (one call a second).
    let one = jobs_answer(vec![job_json("only", "cli", "active", false)]);
    t.step(Input::Jobs(out_before, jobs_reply(one)));
    let again = load_of(&t.after(1_000)).expect("a new call");
    assert_eq!(t.page().rows.len(), 3, "the old answer is not shown");
    assert_eq!(t.page().load, Load::Failed(STALE.into()));
    t.step(Input::Jobs(again, jobs_reply(three())));
    assert_eq!(t.view().counts(), Some((1, 2)));
}

#[test]
fn a_jump_of_a_few_seconds_is_not_a_suspend() {
    let mut t = T::known();
    t.jump(4_000);
    assert_eq!(t.page().load, Load::Ready);
}

#[test]
fn an_answer_nobody_asked_for_changes_nothing() {
    let mut t = T::known();
    let (call, _) = t.next_load(40_000).expect("a refresh");
    let only = jobs_answer(vec![job_json("x", "cli", "active", false)]);
    let stranger = {
        // A call id this engine never gave out.
        let mut other = T::new();
        load_of(&other.start()).expect("first")
    };
    let out = t.step(Input::Jobs(stranger, jobs_reply(only)));
    assert!(!out.changed, "a stranger frees nothing and says nothing");
    assert_eq!(t.page().rows.len(), 3);
    t.step(Input::Jobs(call, jobs_reply(three())));
    let out = t.step(Input::Jobs(call, jobs_reply(three())));
    assert!(!out.changed, "a repeat is nothing");
}

#[test]
fn what_is_left_out_of_the_answer_is_said_and_no_count_is_drawn() {
    let mut t = T::new();
    let call = load_of(&t.start()).expect("first");
    let many: Vec<_> = (0..600)
        .map(|i| job_json(&format!("j{i}"), "cli", "active", false))
        .collect();
    t.step(Input::Jobs(call, jobs_reply(jobs_answer(many))));
    let v = t.view();
    assert_eq!((v.page.rows.len(), v.left_out), (500, 100));
    assert_eq!(v.counts(), None, "500 on of 600 would be a lie");
}

#[test]
fn a_selection_that_the_list_no_longer_has_closes_its_history() {
    let mut t = T::known();
    t.open("on", &["ok"]);
    assert_eq!(t.view().history.load, Load::Ready);
    let (call, _) = t.next_load(40_000).expect("refresh");
    let without: CronJobs = jobs_answer(vec![job_json("off", "cli", "paused", false)]);
    t.step(Input::Jobs(call, jobs_reply(without)));
    let v = t.view();
    assert!(v.page.selected.is_none() && v.page.history.is_empty());
    assert_eq!(v.history.load, Load::Idle);
}

#[test]
fn a_dead_thread_ends_the_driver() {
    let mut t = T::known();
    assert!(t.step(Input::ThreadFailed).fatal);
}

#[test]
fn a_panic_while_the_state_was_locked_throws_it_away_and_asks_again() {
    let mut t = T::known();
    let store = std::sync::Arc::clone(&t.store);
    let _ = std::thread::spawn(move || {
        let _guard = store.lock();
        panic!("a panic with the state locked");
    })
    .join();
    let out = t.after(2_000);
    assert!(out.changed);
    let v = t.view();
    assert!(
        v.page.rows.is_empty() && v.page.load != Load::Ready,
        "not loaded"
    );
    assert!(load_of(&out).is_some(), "and asked for again");
}
