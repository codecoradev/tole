//! The driver end to end: real threads, the real model, a scripted [`FakeApi`]
//! and a [`ManualClock`], so no socket and no real sleep. Waits are on
//! conditions (a call arrived, the view changed), bounded in real time only so
//! that a broken driver fails a test instead of hanging it.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use tole_desktop_model::{Badge, Load, Tab};

use super::engine::STALE;
use super::fixtures::{io_err, skills, status, tools, T0};
use super::rig::{count, known, Rig};
use super::{Config, Handle};
use crate::driver::fake::FakeApi;

#[test]
fn the_page_is_not_loaded_until_the_answers_arrive() {
    let api = FakeApi::new();
    let (gate_skills, gate_tools) = (api.gate_skills(), api.gate_tools());
    let rig = Rig::start(api.clone());
    api.wait_skills(1);
    api.wait_tools(1);
    let v = rig.handle.snapshot();
    assert_eq!(v.skills_waiting, Badge::Unknown);
    assert_eq!(v.tools_needing_attention, Badge::Unknown);
    assert_eq!(v.page.skills_load, Load::Loading);
    assert!(v.skills_feed.loading && v.tools_feed.loading);
    assert!(rig.handle.is_running());

    gate_skills.send(Ok(skills(2))).unwrap();
    let v = rig.until("the skills are known", |v| {
        v.skills_waiting == Badge::Count(2)
    });
    assert_eq!(v.tools_needing_attention, Badge::Unknown, "the other tab");
    gate_tools.send(Ok(tools(1))).unwrap();
    let v = rig.until("the tools are known", |v| count(v, 2, 1));
    assert_eq!(v.page.skills.review.len(), 2);
    assert_eq!(v.page.tools.len(), 2);
    assert!(!v.skills_feed.loading && !v.tools_feed.loading);
}

#[test]
fn the_owner_can_refresh_and_the_new_answer_replaces_the_old() {
    let (api, rig) = known(0);
    api.reply_skills(Ok(skills(5)));
    api.reply_tools(Ok(tools(0)));
    rig.handle.refresh().unwrap();
    rig.tick_to_skills(&api, 2);
    rig.until("the answers replace the old", |v| count(v, 5, 0));
}

#[test]
fn a_slow_timer_refreshes_and_the_rows_stay_while_the_call_is_out() {
    let (api, rig) = known(0);
    let gate = api.gate_skills();
    api.reply_tools(Ok(tools(1)));
    rig.advance(Duration::from_secs(29));
    rig.sync();
    assert_eq!(api.skills_calls(), 1, "not before the interval");
    rig.advance(Duration::from_secs(1));
    api.wait_skills(2);
    let v = rig.until("a call is out", |v| v.skills_feed.loading);
    assert_eq!(v.skills_waiting, Badge::Count(2), "the rows are not hidden");
    gate.send(Ok(skills(4))).unwrap();
    rig.until("the refresh landed", |v| {
        v.skills_waiting == Badge::Count(4)
    });
}

#[test]
fn presses_while_a_call_is_out_are_one_more_call() {
    let (api, rig) = known(0);
    let gate = api.gate_skills();
    api.reply_skills(Ok(skills(3)));
    for _ in 0..6 {
        api.reply_tools(Ok(tools(1)));
    }
    rig.handle.refresh().unwrap();
    rig.tick_to_skills(&api, 2);
    for _ in 0..10 {
        rig.handle.refresh().unwrap();
    }
    rig.tick_for(5);
    assert_eq!(api.skills_calls(), 2, "one call of a route at a time");
    gate.send(Ok(skills(2))).unwrap();
    api.wait_skills(3); // no time needed: the pace has passed
    rig.until("the repeat landed", |v| v.skills_waiting == Badge::Count(3));
    rig.tick_for(5);
    assert_eq!(api.skills_calls(), 3, "ten presses were one call");
}

#[test]
fn a_refused_or_broken_call_empties_the_tab_and_is_retried_until_it_works() {
    let errors = [
        status(404, "no such route"),
        status(429, "slow down"),
        status(500, "boom"),
        io_err(std::io::ErrorKind::ConnectionReset),
    ];
    for error in errors {
        let (api, rig) = known(0);
        api.reply_skills(Err(error));
        api.reply_skills(Ok(skills(7)));
        api.reply_tools(Ok(tools(1)));
        rig.handle.refresh().unwrap();
        rig.tick_to_skills(&api, 2);
        let v = rig.until("the failure shows", |v| v.skills_feed.failures == 1);
        assert_eq!(v.skills_waiting, Badge::Unknown, "never a zero");
        assert!(v.page.skills.review.is_empty() && v.page.skills.approved.is_empty());
        assert!(matches!(v.page.skills_load, Load::Failed(_)));
        assert_eq!(v.tools_needing_attention, Badge::Count(1), "not this tab");
        // The retry comes after the backoff wait, and works.
        rig.tick_to_skills(&api, 3);
        let v = rig.until("known again", |v| v.skills_waiting == Badge::Count(7));
        assert_eq!(v.skills_feed.failures, 0);
    }
}

#[test]
fn a_panic_in_a_call_is_that_calls_failure_and_the_driver_goes_on() {
    let (api, rig) = known(0);
    api.panic_skills();
    api.reply_skills(Ok(skills(1)));
    api.panic_tools();
    api.reply_tools(Ok(tools(0)));
    rig.handle.refresh().unwrap();
    rig.tick_to_skills(&api, 2);
    let v = rig.until("both failed", |v| {
        v.skills_feed.failures == 1 && v.tools_feed.failures == 1
    });
    assert_eq!(v.skills_waiting, Badge::Unknown);
    assert_eq!(v.tools_needing_attention, Badge::Unknown);
    assert!(rig.handle.is_running());
    rig.tick_to_skills(&api, 3);
    rig.until("the retries landed", |v| count(v, 1, 0));
}

#[test]
fn a_suspend_takes_the_rows_back_until_a_fresh_call_answers() {
    let (api, rig) = known(0);
    let (gate_skills, gate_tools) = (api.gate_skills(), api.gate_tools());
    // An hour on the wall clock, a second on the monotonic one.
    rig.clock.set_wall_ms(T0 + 3_600_000);
    rig.tick();
    // Taken back, and a fresh call out (the two happen in one step, but a view
    // read between them is possible, so wait for the end state).
    let v = rig.until("taken back and asked again", |v| {
        v.page.skills_load == Load::Loading && v.page.tools_load == Load::Loading
    });
    assert_eq!(v.skills_waiting, Badge::Unknown);
    assert_eq!(v.tools_needing_attention, Badge::Unknown);
    assert!(v.page.skills.review.is_empty());
    gate_skills.send(Ok(skills(1))).unwrap();
    gate_tools.send(Ok(tools(0))).unwrap();
    rig.until("known again", |v| count(v, 1, 0));
}

#[test]
fn a_call_that_was_out_during_a_suspend_is_not_shown_when_it_answers() {
    let (api, rig) = known(0);
    let late = api.gate_skills();
    api.reply_skills(Ok(skills(1)));
    for _ in 0..3 {
        api.reply_tools(Ok(tools(1)));
    }
    rig.advance(Duration::from_secs(30));
    api.wait_skills(2);
    rig.clock.set_wall_ms(T0 + 3_600_000);
    rig.tick();
    rig.until("taken back", |v| v.skills_waiting == Badge::Unknown);
    let saw_old = AtomicBool::new(false);
    late.send(Ok(skills(9))).unwrap();
    api.wait_skills(3);
    rig.until("the fresh call landed", |v| {
        saw_old.fetch_or(v.skills_waiting == Badge::Count(9), Ordering::SeqCst);
        v.skills_waiting == Badge::Count(1)
    });
    assert!(
        !saw_old.load(Ordering::SeqCst),
        "the old answer never showed"
    );
}

#[test]
fn rows_older_than_twice_the_refresh_are_taken_back_and_a_late_answer_restores_them() {
    let (api, rig) = known(0);
    let slow = api.gate_skills();
    for _ in 0..3 {
        api.reply_tools(Ok(tools(1)));
    }
    rig.advance(Duration::from_secs(30));
    api.wait_skills(2);
    // The tools answered that refresh before the clock moves on again: a time
    // step past an answer that is still on its way ages the rows, by design.
    api.wait_tools(2);
    rig.until("the tools are refreshed", |v| !v.tools_feed.loading);
    rig.advance(Duration::from_secs(31));
    let v = rig.until("too old to show", |v| v.skills_waiting == Badge::Unknown);
    assert_eq!(v.page.skills_load, Load::Failed(STALE.into()));
    assert!(v.skills_feed.loading, "the call is still awaited");
    assert_eq!(v.tools_needing_attention, Badge::Count(1), "its own age");
    slow.send(Ok(skills(4))).unwrap();
    rig.until("the late answer is fresh", |v| {
        v.skills_waiting == Badge::Count(4)
    });
}

#[test]
fn the_ui_callback_runs_with_no_lock_held() {
    let slot: Arc<Mutex<Option<Handle>>> = Arc::default();
    let reads = Arc::new(AtomicUsize::new(0));
    let changes = Arc::new((Mutex::new(0u64), Condvar::new()));
    let cfg = {
        let (slot, reads, changes) = (slot.clone(), reads.clone(), changes.clone());
        Config {
            on_change: Some(Arc::new(move || {
                // Reading the view from inside the callback takes the state
                // lock: it would hang if the worker still held it.
                if let Some(h) = &*slot.lock().unwrap() {
                    let _ = h.snapshot();
                    reads.fetch_add(1, Ordering::SeqCst);
                }
                *changes.0.lock().unwrap() += 1;
                changes.1.notify_all();
            })),
            ..Config::default()
        }
    };
    let api = FakeApi::new();
    api.reply_skills(Ok(skills(2)));
    api.reply_tools(Ok(tools(1)));
    let rig = Rig::start_with(api, cfg, changes);
    *slot.lock().unwrap() = Some(rig.handle.clone());
    rig.until("known", |v| count(v, 2, 1));
    rig.sync();
    assert!(reads.load(Ordering::SeqCst) > 0);
}

#[test]
fn the_selected_tab_is_a_command_and_survives_a_refresh() {
    let (api, rig) = known(0);
    api.reply_skills(Err(io_err(std::io::ErrorKind::TimedOut)));
    api.reply_tools(Ok(tools(1)));
    rig.handle.select_tab(Tab::ConnectedTools).unwrap();
    rig.until("selected", |v| v.page.tab == Tab::ConnectedTools);
    rig.handle.refresh().unwrap();
    rig.tick_to_skills(&api, 2);
    rig.until("failed", |v| v.skills_feed.failures == 1);
    assert_eq!(rig.handle.snapshot().page.tab, Tab::ConnectedTools);
}

#[test]
fn a_flood_of_commands_costs_no_calls_while_time_stands_still_and_never_hangs() {
    let (api, rig) = known(0);
    let pressers: Vec<_> = (0..4)
        .map(|n| {
            let handle = rig.handle.clone();
            std::thread::spawn(move || {
                for i in 0..5_000 {
                    // A full inbox is `Busy`: the press is dropped, never a wait.
                    let _ = if (i + n) % 2 == 0 {
                        handle.refresh()
                    } else {
                        handle.select_tab(Tab::ConnectedTools)
                    };
                }
            })
        })
        .collect();
    for p in pressers {
        p.join().expect("a presser");
    }
    rig.sync();
    assert!(rig.handle.is_running());
    assert_eq!(
        api.skills_calls(),
        1,
        "one call a second, and no second passed"
    );
    assert_eq!(api.tools_calls(), 1);
    assert!(count(&rig.handle.snapshot(), 2, 1));
}

#[test]
fn a_ui_callback_that_panics_does_not_take_the_worker_down() {
    let api = FakeApi::new();
    api.reply_skills(Ok(skills(2)));
    api.reply_tools(Ok(tools(1)));
    let calls = Arc::new(AtomicUsize::new(0));
    let cfg = {
        let calls = calls.clone();
        Config {
            on_change: Some(Arc::new(move || {
                calls.fetch_add(1, Ordering::SeqCst);
                panic!("the UI's callback fails");
            })),
            ..Config::default()
        }
    };
    let rig = Rig::start_with(api, cfg, Arc::default());
    let end = std::time::Instant::now() + Duration::from_secs(10);
    while !count(&rig.handle.snapshot(), 2, 1) && std::time::Instant::now() < end {
        std::thread::yield_now();
    }
    assert!(
        count(&rig.handle.snapshot(), 2, 1),
        "the answers were applied"
    );
    assert!(rig.handle.is_running());
    assert!(calls.load(Ordering::SeqCst) > 0);
}
