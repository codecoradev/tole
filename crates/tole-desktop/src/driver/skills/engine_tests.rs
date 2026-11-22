//! The engine's rules, one input at a time, with no thread and no clock: what
//! it asks for first, how it paces and retries, when it takes rows back and
//! which answers it ignores.

use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::Arc;
use std::time::Duration;

use tole_desktop_model::{Badge, Load, Tab};

use super::engine::{Input, Reply, Route, STALE};
use super::fixtures::{
    load_of, skills_err, skills_from, skills_ok, status, tools_err, tools_from, tools_ok, T,
};

#[test]
fn start_asks_for_both_routes_at_once_and_nothing_is_known() {
    let mut t = T::new();
    let out = t.start();
    assert!(load_of(&out, Route::Skills).is_some());
    assert!(load_of(&out, Route::Tools).is_some());
    assert_eq!(out.effects.len(), 2);
    let v = t.view();
    assert_eq!(v.page.skills_load, Load::Loading);
    assert_eq!(v.page.tools_load, Load::Loading);
    assert_eq!(v.skills_waiting, Badge::Unknown);
    assert_eq!(v.tools_needing_attention, Badge::Unknown);
    assert!(v.skills_feed.loading && v.tools_feed.loading);
    assert!(!v.page.skills_inbox_is_empty() && !v.page.no_connected_tools());
}

#[test]
fn the_answers_make_each_tab_known_and_free_its_slot() {
    let mut t = T::new();
    let out = t.start();
    let out = t.step(Input::Loaded(
        load_of(&out, Route::Skills).unwrap(),
        skills_ok(2),
    ));
    assert!(out.changed);
    let v = t.view();
    assert_eq!(v.skills_waiting, Badge::Count(2));
    assert_eq!(v.page.skills_load, Load::Ready);
    assert!(!v.skills_feed.loading && v.tools_feed.loading);
    assert_eq!(v.tools_needing_attention, Badge::Unknown, "the other tab");
    assert_eq!(v.page.skills.review.len(), 2);
    assert_eq!(v.page.skills.approved.len(), 1);
}

#[test]
fn a_failed_call_empties_its_tab_and_is_retried_with_a_growing_wait_for_ever() {
    let mut t = T::new();
    let out = t.start();
    let mut id = load_of(&out, Route::Skills).unwrap();
    // 1 s, 2 s, 4 s, 8 s, 16 s, then 30 s for as long as it fails.
    for (n, wait) in [1, 2, 4, 8, 16, 30, 30, 30].into_iter().enumerate() {
        t.step(Input::Loaded(id, skills_err()));
        let v = t.view();
        assert_eq!(v.skills_feed.failures, n as u32 + 1);
        assert!(matches!(v.page.skills_load, Load::Failed(_)));
        assert_eq!(v.skills_waiting, Badge::Unknown);
        assert!(t.next_load(Route::Skills, wait * 1_000 - 250).is_none());
        let (next, spent) = t.next_load(Route::Skills, 250).expect("the retry");
        assert_eq!(spent, 250);
        id = next;
    }
    t.step(Input::Loaded(id, skills_ok(0)));
    let v = t.view();
    assert_eq!(v.skills_feed.failures, 0);
    assert_eq!(v.skills_waiting, Badge::Count(0), "a complete empty read");
    assert!(v.page.skills_inbox_is_empty());
}

#[test]
fn a_tab_that_fails_does_not_hold_back_or_blank_the_other() {
    let mut t = T::new();
    let out = t.start();
    t.step(Input::Loaded(
        load_of(&out, Route::Skills).unwrap(),
        skills_ok(1),
    ));
    t.step(Input::Loaded(
        load_of(&out, Route::Tools).unwrap(),
        tools_err(),
    ));
    let v = t.view();
    assert_eq!(v.skills_waiting, Badge::Count(1));
    assert_eq!(v.tools_needing_attention, Badge::Unknown);
    assert_eq!(v.tools_feed.failures, 1);
    assert_eq!(v.skills_feed.failures, 0);
    // The tools retry on their own backoff; the skills wait for their refresh.
    assert!(t.next_load(Route::Tools, 1_000).is_some());
    assert!(t.next_load(Route::Skills, 20_000).is_none());
}

#[test]
fn a_failed_refresh_takes_the_rows_back_and_a_failure_is_never_a_zero() {
    let mut t = T::known();
    let (id, _) = t.next_load(Route::Skills, 31_000).expect("the refresh");
    t.step(Input::Loaded(
        id,
        Reply::Skills(Err(status(404, "no such route"))),
    ));
    let v = t.view();
    assert!(v.page.skills.review.is_empty() && v.page.skills.approved.is_empty());
    assert_eq!(v.skills_waiting, Badge::Unknown, "not Count(0)");
    assert!(!v.page.skills_inbox_is_empty());
    assert!(matches!(&v.page.skills_load, Load::Failed(m) if m.contains("404")));
    assert_eq!(v.tools_needing_attention, Badge::Count(1), "untouched");
}

#[test]
fn a_partial_answer_is_unknown_and_never_nothing_to_review() {
    let mut t = T::new();
    let out = t.start();
    let partial = skills_from(vec![], vec![], &["drafts not read: bad file"]);
    t.step(Input::Loaded(
        load_of(&out, Route::Skills).unwrap(),
        Reply::Skills(Ok(partial)),
    ));
    let tools = tools_from(vec![], &["plugins not read: x"]);
    t.step(Input::Loaded(
        load_of(&out, Route::Tools).unwrap(),
        Reply::Tools(Ok(tools)),
    ));
    let v = t.view();
    assert_eq!(v.page.skills_load, Load::Ready);
    assert_eq!(v.skills_waiting, Badge::Unknown);
    assert!(!v.page.skills_inbox_is_empty());
    assert_eq!(v.page.skills.notes, ["drafts not read: bad file"]);
    assert_eq!(v.tools_needing_attention, Badge::Unknown);
    assert!(!v.page.no_connected_tools());
}

#[test]
fn a_refresh_comes_on_the_slow_timer_and_the_rows_stay_while_it_runs() {
    let mut t = T::known();
    assert!(t.next_load(Route::Skills, 29_750).is_none(), "not before");
    let out = t.after(250);
    let id = load_of(&out, Route::Skills).expect("30 s after the answer");
    let v = t.view();
    assert!(v.skills_feed.loading);
    assert_eq!(v.page.skills_load, Load::Ready, "the rows are not hidden");
    assert_eq!(v.skills_waiting, Badge::Count(2), "no flicker to Unknown");
    // The next one is 30 s after this answer, not after the last start.
    t.after(10_000);
    t.step(Input::Loaded(id, skills_ok(3)));
    assert_eq!(t.view().skills_waiting, Badge::Count(3));
    assert!(t.next_load(Route::Skills, 29_750).is_none());
    assert!(t.next_load(Route::Skills, 250).is_some());
}

#[test]
fn the_pace_is_one_call_a_second_whatever_asks() {
    let mut t = T::known();
    let out = t.step(Input::Refresh);
    assert!(
        load_of(&out, Route::Skills).is_none(),
        "the answer came the instant the call began"
    );
    let (_, spent) = t.next_load(Route::Skills, 2_000).expect("the refresh");
    assert_eq!(spent, 1_000, "one second after the last start");
}

#[test]
fn the_owner_refresh_is_one_more_call_however_often_pressed_and_not_one_in_flight() {
    let mut t = T::known();
    let (id, _) = t.next_load(Route::Skills, 31_000).expect("the timer");
    // Pressed while that call is out: it may have read before the change the
    // owner waits for, so one more call follows its answer.
    for _ in 0..10 {
        t.step(Input::Refresh);
    }
    assert!(t.next_load(Route::Skills, 5_000).is_none(), "one at a time");
    let out = t.step(Input::Loaded(id, skills_ok(2)));
    let second = load_of(&out, Route::Skills).expect("one more call follows the answer");
    t.step(Input::Loaded(second, skills_ok(2)));
    assert!(
        t.next_load(Route::Skills, 29_000).is_none(),
        "ten presses are one call, not ten"
    );
}

#[test]
fn the_owner_refresh_skips_the_backoff_wait_but_not_the_pace() {
    let mut t = T::new();
    let out = t.start();
    let mut id = load_of(&out, Route::Skills).unwrap();
    for _ in 0..5 {
        t.step(Input::Loaded(id, skills_err()));
        id = t.next_load(Route::Skills, 40_000).expect("a retry").0;
    }
    t.step(Input::Loaded(id, skills_err()));
    // The sixth wait is 32 s capped at 30 s; the owner presses at once.
    t.step(Input::Refresh);
    let (_, spent) = t.next_load(Route::Skills, 5_000).expect("at once");
    assert_eq!(spent, 1_000, "after the pace, not the backoff");
}

#[test]
fn an_answer_after_the_owners_refresh_ends_the_backoff_wait() {
    // A short refresh, so that a wait left over from the failures would show.
    let mut t = T::with_refresh(Duration::from_secs(5));
    let out = t.start();
    let mut id = load_of(&out, Route::Skills).unwrap();
    for _ in 0..5 {
        t.step(Input::Loaded(id, skills_err()));
        id = t.next_load(Route::Skills, 40_000).expect("a retry").0;
    }
    t.step(Input::Loaded(id, skills_err()));
    t.step(Input::Refresh);
    let (id, _) = t.next_load(Route::Skills, 5_000).expect("the owner's call");
    t.step(Input::Loaded(id, skills_ok(1)));
    let (_, spent) = t.next_load(Route::Skills, 6_000).expect("the timer");
    assert_eq!(spent, 5_000, "on the timer, not after the old 30 s wait");
    assert_eq!(t.view().skills_waiting, Badge::Count(1), "never too old");
}

#[test]
fn rows_older_than_twice_the_refresh_are_taken_back_and_a_late_answer_restores_them() {
    let mut t = T::known();
    let (id, _) = t.next_load(Route::Skills, 31_000).expect("the refresh");
    // The call hangs. 60 s after the rows were read they are still shown.
    t.after(29_000);
    assert_eq!(t.view().skills_waiting, Badge::Count(2));
    t.after(2_000);
    let v = t.view();
    assert_eq!(v.skills_waiting, Badge::Unknown, "too old to show");
    assert_eq!(v.page.skills_load, Load::Failed(STALE.into()));
    assert!(v.page.skills.review.is_empty());
    assert!(v.skills_feed.loading, "the call is still awaited");
    // It answers after all: that answer is fresh.
    t.step(Input::Loaded(id, skills_ok(4)));
    assert_eq!(t.view().skills_waiting, Badge::Count(4));
}

#[test]
fn a_suspend_takes_the_rows_back_and_ignores_the_call_that_was_out() {
    let mut t = T::known();
    let (id, _) = t.next_load(Route::Skills, 31_000).expect("the refresh");
    // An hour passes on the wall clock and 1 s on the monotonic one.
    t.now.mono += 1_000;
    let out = t.jump(3_600_000);
    assert!(out.changed);
    let v = t.view();
    assert_eq!(v.skills_waiting, Badge::Unknown);
    assert_eq!(v.tools_needing_attention, Badge::Unknown);
    assert_eq!(v.page.skills_load, Load::Failed(STALE.into()));
    // The call that was out read before the sleep: its answer shows nothing,
    // but it frees the slot and a fresh call follows.
    let out = t.step(Input::Loaded(id, skills_ok(9)));
    let v = t.view();
    assert_eq!(
        v.skills_waiting,
        Badge::Unknown,
        "the old answer is ignored"
    );
    assert!(v.page.skills.review.is_empty());
    let fresh = load_of(&out, Route::Skills)
        .or_else(|| t.next_load(Route::Skills, 2_000).map(|(i, _)| i))
        .expect("a call after the sleep");
    t.step(Input::Loaded(fresh, skills_ok(1)));
    assert_eq!(t.view().skills_waiting, Badge::Count(1));
}

#[test]
fn a_suspend_during_the_first_load_ignores_its_answer_and_asks_again() {
    let mut t = T::new();
    let out = t.start();
    let (s, g) = (
        load_of(&out, Route::Skills).unwrap(),
        load_of(&out, Route::Tools).unwrap(),
    );
    t.jump(7_200_000);
    assert_eq!(t.view().page.skills_load, Load::Loading, "nothing to take");
    t.step(Input::Loaded(s, skills_ok(5)));
    t.step(Input::Loaded(g, tools_ok(0)));
    assert_eq!(t.view().skills_waiting, Badge::Unknown);
    let (s2, _) = t.next_load(Route::Skills, 2_000).expect("asked again");
    t.step(Input::Loaded(s2, skills_ok(1)));
    assert_eq!(t.view().skills_waiting, Badge::Count(1));
}

#[test]
fn a_wall_clock_set_back_is_not_a_loss_and_a_small_step_is_not_either() {
    let mut t = T::known();
    t.now.wall -= 3_600_000;
    t.step(Input::Tick);
    assert_eq!(t.view().skills_waiting, Badge::Count(2));
    t.jump(4_000);
    assert_eq!(t.view().skills_waiting, Badge::Count(2));
    t.jump(6_000);
    assert_eq!(
        t.view().skills_waiting,
        Badge::Unknown,
        "6 s ahead is a sleep"
    );
}

#[test]
fn a_repeated_or_stranger_answer_frees_nothing_and_changes_nothing() {
    let mut t = T::known();
    let (first, _) = t.next_load(Route::Skills, 31_000).expect("the refresh");
    t.step(Input::Loaded(first, skills_ok(2)));
    let (second, _) = t.next_load(Route::Skills, 31_000).expect("the next");
    assert_ne!(first, second);
    let before = t.view();
    // The first answer again, with other data, while the second call is out.
    let out = t.step(Input::Loaded(first, skills_ok(7)));
    assert!(!out.changed);
    assert_eq!(t.view(), before);
    assert!(t.view().skills_feed.loading, "the slot is still taken");
    assert!(t.next_load(Route::Skills, 5_000).is_none());
    t.step(Input::Loaded(second, skills_ok(3)));
    assert_eq!(t.view().skills_waiting, Badge::Count(3));
}

#[test]
fn a_thrown_away_state_is_loaded_again_and_nothing_is_known_meanwhile() {
    let mut t = T::known();
    let store = Arc::clone(&t.store);
    let died = catch_unwind(AssertUnwindSafe(|| {
        let _held = store.lock();
        panic!("a panic while the lock is held");
    }));
    assert!(died.is_err());
    // The next look finds the poison and starts from nothing.
    assert_eq!(t.view().skills_waiting, Badge::Unknown);
    let out = t.after(1_000);
    assert!(out.changed);
    assert!(load_of(&out, Route::Skills).is_some() && load_of(&out, Route::Tools).is_some());
    assert_eq!(t.view().skills_waiting, Badge::Unknown);
}

#[test]
fn the_selected_tab_is_kept_across_loads() {
    let mut t = T::known();
    assert!(t.step(Input::SelectTab(Tab::ConnectedTools)).changed);
    assert!(!t.step(Input::SelectTab(Tab::ConnectedTools)).changed);
    let (id, _) = t.next_load(Route::Skills, 31_000).expect("the refresh");
    t.step(Input::Loaded(id, skills_err()));
    assert_eq!(t.view().page.tab, Tab::ConnectedTools);
}

#[test]
fn a_thread_that_died_ends_the_driver() {
    let mut t = T::known();
    assert!(t.step(Input::ThreadFailed).fatal);
}
