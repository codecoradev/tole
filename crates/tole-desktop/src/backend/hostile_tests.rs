//! What tries to break the backend: panics in a callback or a driver, a stalled
//! callback, a storm of changes, calls from many threads while the serve flaps,
//! and shutdown in the middle of it.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc};
use std::time::{Duration, Instant};

use super::fake::{restarting, running, FakeLink};
use super::rig::{is_known, is_unknown, known_api, no_approvals, Rig, PATIENCE};
use super::{OpenRefused, TaskRefused};
use crate::driver::clock::{Clock, ManualClock, Shutdown, Slept};
use crate::driver::fake::FakeApi;
use crate::driver::shared::handle::OnChange;
use crate::driver::task::MAX_OPEN_TASKS;

/// A clock whose `sleep` panics, once told to, on the threads with these names:
/// those threads die outside any call.
struct Killer {
    inner: Arc<ManualClock>,
    victims: &'static [&'static str],
    kill: AtomicBool,
}

impl Clock for Killer {
    fn now_ms(&self) -> u64 {
        self.inner.now_ms()
    }

    fn monotonic_ms(&self) -> u64 {
        self.inner.monotonic_ms()
    }

    fn sleep(&self, d: Duration, stop: &Shutdown) -> Slept {
        let thread = std::thread::current();
        let victim = thread.name().is_some_and(|n| self.victims.contains(&n));
        assert!(
            !(victim && self.kill.load(Ordering::SeqCst)),
            "a thread dies outside any call"
        );
        self.inner.sleep(d, stop)
    }
}

fn killer(rig_clock: &Arc<ManualClock>, victims: &'static [&'static str]) -> Arc<Killer> {
    Arc::new(Killer {
        inner: Arc::clone(rig_clock),
        victims,
        kill: AtomicBool::new(false),
    })
}

fn changes(rig: &Rig) -> u64 {
    rig.changes.load(Ordering::SeqCst)
}

fn wait_for(why: &str, ok: impl Fn() -> bool) {
    let end = Instant::now() + PATIENCE;
    while !ok() {
        assert!(Instant::now() < end, "timed out waiting for: {why}");
        std::thread::sleep(Duration::from_millis(1));
    }
}

#[test]
fn a_storm_of_changes_is_one_call_to_the_ui_per_100_ms_of_the_clock() {
    let link = FakeLink::ready();
    link.queue_api(known_api(0, 0));
    let rig = Rig::start(link);
    rig.until("known", |v| is_known(v, 0, 0));
    // The first change is told at once, with the clock where it started.
    wait_for("the first change", || changes(&rig) >= 1);
    let backend = rig.backend();
    // The clock stands still, so every change after the first is "too soon".
    std::thread::scope(|s| {
        for n in 0..4 {
            s.spawn(move || {
                for i in 0..500 {
                    let id = format!("t{n}-{}", i % 2);
                    let _ = backend.open_task(&id);
                    backend.close_task(&id);
                }
            });
        }
    });
    let storm = changes(&rig);
    assert!(
        (1..=2).contains(&storm),
        "{storm} calls for thousands of changes"
    );
    // Time passes: the change that waited is told once.
    rig.advance(Duration::from_millis(100));
    wait_for("the change that waited", || changes(&rig) > storm);
    // Two seconds of the clock, 20 ticks: the drivers go on changing (the task
    // drivers retry their reads), and the UI hears at most once per tick.
    rig.advance(Duration::from_secs(2));
    let settled = changes(&rig);
    assert!(
        settled <= storm + 21,
        "{settled} calls after {storm}: at most one per tick"
    );
}

#[test]
fn a_panic_in_the_ui_callback_does_not_stop_the_backend() {
    let link = FakeLink::ready();
    link.queue_api(known_api(1, 0));
    link.queue_api(known_api(2, 0));
    let clock = Arc::new(ManualClock::new(super::rig::T0));
    let boom: OnChange = Arc::new(|| panic!("the UI's callback panics"));
    let rig = Rig::start_on(link.clone(), clock.clone(), clock, Some(boom));
    rig.until("known", |v| is_known(v, 1, 0));
    link.set(restarting());
    link.set(running("2"));
    rig.until_ticking("new drivers", |v| is_known(v, 2, 0));
    assert!(rig.backend().is_running());
    assert!(
        changes(&rig) >= 2,
        "it was called, and it panicked each time"
    );
}

#[test]
fn a_callback_that_never_returns_does_not_block_the_view_or_the_commands() {
    let link = FakeLink::ready();
    link.queue_api(known_api(0, 0));
    let (release, gate) = mpsc::channel::<()>();
    let gate = std::sync::Mutex::new(gate);
    let stuck: OnChange = Arc::new(move || {
        let _ = gate.lock().unwrap().recv();
    });
    let clock = Arc::new(ManualClock::new(super::rig::T0));
    let rig = Rig::start_on(link.clone(), clock.clone(), clock, Some(stuck));
    rig.until("known", |v| is_known(v, 0, 0));
    // The worker is inside the callback now; the UI's side does not wait for it.
    let started = Instant::now();
    let b = rig.backend();
    b.open_task("a").unwrap();
    b.open_task("b").unwrap();
    assert!(b.close_task("a"));
    assert_eq!(b.snapshot().open_tasks, ["b"]);
    assert!(b.with_approvals(|h| h.is_running()).is_some());
    assert!(started.elapsed() < Duration::from_secs(2));
    drop(release);
}

#[test]
fn a_driver_that_dies_is_replaced_and_not_in_a_loop() {
    let link = FakeLink::ready();
    for _ in 0..8 {
        link.queue_api(known_api(1, 0));
    }
    let clock = Arc::new(ManualClock::new(super::rig::T0));
    let dying = killer(&clock, &["approvals-tick", "skills-tick"]);
    dying.kill.store(true, Ordering::SeqCst);
    let rig = Rig::start_on(link.clone(), clock.clone(), dying.clone(), None);
    // They die as they start; the backend notices and tries again, later.
    link.wait_api_calls(1);
    rig.wait_ticking("the dead drivers were replaced", |_| link.api_calls() >= 2);
    // Ten seconds more of drivers that die at once: 1 s, 2 s, 4 s waits, not a loop.
    rig.advance(Duration::from_secs(10));
    assert!(link.api_calls() <= 5, "{} attaches", link.api_calls());
    // Healthy drivers stay.
    dying.kill.store(false, Ordering::SeqCst);
    rig.until_ticking("known", |v| is_known(v, 1, 0));
    assert!(rig.backend().is_running());
}

#[test]
fn a_ticker_that_dies_ends_the_backend_cleanly() {
    let link = FakeLink::ready();
    link.queue_api(known_api(0, 0));
    let clock = Arc::new(ManualClock::new(super::rig::T0));
    let dying = killer(&clock, &["backend-tick"]);
    let rig = Rig::start_on(link.clone(), clock.clone(), dying.clone(), None);
    rig.until("known", |v| is_known(v, 0, 0));
    dying.kill.store(true, Ordering::SeqCst);
    // Wakes the ticker; its next sleep panics (so it never sleeps again).
    rig.clock.advance(Duration::from_millis(100));
    wait_for("the backend ended", || !rig.backend().is_running());
    let b = rig.backend();
    assert!(
        is_unknown(&b.snapshot()),
        "the view reads unknown, not stale"
    );
    assert_eq!(b.open_task("a"), Err(OpenRefused::Stopped));
    assert_eq!(b.send("a", "x"), Err(TaskRefused::NotOpen));
    assert!(b.with_approvals(|_| ()).is_none());
    assert_eq!(b.thread_report().panicked, 0);
}

#[test]
fn many_threads_opening_and_closing_never_run_more_than_the_cap() {
    let link = FakeLink::ready();
    let rig = Rig::start(link.clone());
    let backend = rig.backend();
    let done = AtomicBool::new(false);
    std::thread::scope(|s| {
        s.spawn(|| {
            while !done.load(Ordering::SeqCst) {
                let view = backend.snapshot();
                assert!(view.open_tasks.len() <= MAX_OPEN_TASKS);
                let look = backend.link.look();
                let running = backend.shared.current(&look).map_or(0, |a| a.tasks.len());
                assert!(running <= MAX_OPEN_TASKS, "{running} task drivers");
            }
        });
        let workers: Vec<_> = (0..4)
            .map(|n| {
                s.spawn(move || {
                    for i in 0..400 {
                        let id = format!("t{}", (n + i) % 6);
                        let _ = backend.open_task(&id);
                        if i % 3 == 0 {
                            backend.close_task(&id);
                        }
                    }
                })
            })
            .collect();
        for w in workers {
            w.join().unwrap();
        }
        done.store(true, Ordering::SeqCst);
    });
    for id in backend.snapshot().open_tasks {
        backend.close_task(&id);
    }
    rig.wait_ticking("every task driver stopped", |b| {
        let look = b.link.look();
        b.shared.current(&look).is_some_and(|a| a.tasks.is_empty())
    });
    assert!(rig.view().open_tasks.is_empty());
}

#[test]
fn a_serve_that_flaps_with_three_open_tasks_ends_with_the_same_three() {
    let link = FakeLink::ready();
    let rig = Rig::start(link.clone());
    let ids = ["s1", "s2", "s3"];
    for id in ids {
        rig.backend().open_task(id).unwrap();
    }
    for i in 0..30_u32 {
        link.set(if i.is_multiple_of(2) {
            restarting()
        } else {
            running("1")
        });
        rig.tick();
    }
    link.set(running("9"));
    for id in ids {
        rig.wait_ticking("a driver", |b| b.task_view(id).is_some());
    }
    assert_eq!(rig.view().open_tasks, ids);
    assert!(link.api_calls() <= 5, "{} attaches", link.api_calls());
}

#[test]
fn commands_while_the_serve_restarts_get_an_answer_and_never_a_panic() {
    let link = FakeLink::ready();
    let rig = Rig::start(link.clone());
    rig.backend().open_task("s1").unwrap();
    rig.wait_ticking("a driver", |b| b.task_view("s1").is_some());
    let backend = rig.backend();
    let answers = std::thread::scope(|s| {
        let sender = s.spawn(|| {
            (0..3_000)
                .map(|_| backend.send("s1", "x").and(backend.stop("s1")))
                .collect::<Vec<_>>()
        });
        for i in 0..40_u32 {
            link.set(if i.is_multiple_of(2) {
                restarting()
            } else {
                running("1")
            });
            rig.tick();
        }
        sender.join().unwrap()
    });
    assert!(answers.iter().all(|a| matches!(
        a,
        Ok(()) | Err(TaskRefused::Unavailable | TaskRefused::Busy)
    )));
}

#[test]
fn shutdown_while_the_serve_flaps_and_tasks_open_returns_in_time() {
    let link = FakeLink::ready();
    let mut rig = Rig::start(link.clone());
    for id in ["s1", "s2", "s3"] {
        rig.backend().open_task(id).unwrap();
    }
    let stop = AtomicBool::new(false);
    let clock = rig.clock.clone();
    let backend = rig.backend.take().expect("running");
    let (report, elapsed) = std::thread::scope(|s| {
        s.spawn(|| {
            let mut i = 0u32;
            while !stop.load(Ordering::SeqCst) {
                link.set(if i.is_multiple_of(2) {
                    restarting()
                } else {
                    running("1")
                });
                i += 1;
                std::thread::yield_now();
            }
        });
        s.spawn(|| {
            while !stop.load(Ordering::SeqCst) {
                clock.advance(Duration::from_millis(100));
            }
        });
        std::thread::sleep(Duration::from_millis(100));
        let begun = Instant::now();
        let report = backend.shutdown(Duration::from_secs(3));
        let elapsed = begun.elapsed();
        stop.store(true, Ordering::SeqCst);
        (report, elapsed)
    });
    assert!(elapsed < Duration::from_secs(4), "{elapsed:?}");
    assert_eq!(report.panicked, 0, "{report:?}");
    // Nothing is attached after it: no more apis are asked for.
    let calls = link.api_calls();
    std::thread::sleep(Duration::from_millis(200));
    assert_eq!(link.api_calls(), calls);
}

#[test]
fn shutdown_with_three_tasks_open_counts_the_readers_it_could_not_interrupt() {
    let link = FakeLink::ready();
    let api: Arc<FakeApi> = known_api(0, 0);
    for _ in 0..8 {
        api.reply_list(Ok(no_approvals()));
    }
    link.queue_api(api.clone());
    let mut rig = Rig::start(link.clone());
    for id in ["s1", "s2", "s3"] {
        rig.backend().open_task(id).unwrap();
        api.wait_opens(id, 1); // the approvals driver's reader is blocked in a stream
    }
    let handles: Vec<_> = (0..3)
        .filter_map(|_| rig.backend().with_approvals(Clone::clone))
        .collect();
    let backend = rig.backend.take().expect("running");
    let begun = Instant::now();
    let report = backend.shutdown(Duration::from_secs(5));
    assert!(
        begun.elapsed() < Duration::from_secs(3),
        "joined in parallel"
    );
    assert_eq!(report.panicked, 0, "{report:?}");
    assert!(
        report.detached >= 3,
        "the readers are in a read: {report:?}"
    );
    assert!(handles.iter().all(|h| !h.is_running()));
}

#[test]
fn a_restart_with_readers_blocked_in_streams_lets_them_go_and_starts_again() {
    let link = FakeLink::ready();
    let first = known_api(0, 0);
    for _ in 0..8 {
        first.reply_list(Ok(no_approvals()));
    }
    link.queue_api(first.clone());
    link.queue_api(known_api(0, 0));
    let rig = Rig::start(link.clone());
    for id in ["s1", "s2", "s3"] {
        rig.backend().open_task(id).unwrap();
        first.wait_opens(id, 1); // the approvals driver's reader is in a read now
    }
    link.set(restarting());
    link.set(running("2"));
    let begun = Instant::now();
    rig.wait_ticking("the readers were let go", |b| {
        b.thread_report().detached >= 3
    });
    rig.wait_ticking("new drivers", |b| {
        ["s1", "s2", "s3"]
            .iter()
            .all(|id| b.task_view(id).is_some())
    });
    assert!(
        begun.elapsed() < Duration::from_secs(4),
        "one bounded wait, not one per driver: {:?}",
        begun.elapsed()
    );
    assert_eq!(rig.view().open_tasks, ["s1", "s2", "s3"]);
    assert_eq!(rig.backend().thread_report().panicked, 0);
}

#[test]
fn a_link_that_forgets_to_bump_its_epoch_is_still_not_ready_in_the_view() {
    let link = FakeLink::ready();
    link.queue_api(known_api(1, 0));
    link.queue_api(known_api(2, 0));
    let rig = Rig::start(link.clone());
    rig.until("known", |v| is_known(v, 1, 0));
    link.set_without_epoch(restarting());
    assert!(is_unknown(&rig.view()), "the state alone says not ready");
    assert!(rig.backend().with_approvals(|_| ()).is_none());
    rig.wait_ticking("the drivers were stopped", |b| {
        b.thread_report().joined >= 13
    });
    link.set_without_epoch(running("2"));
    rig.until_ticking("new drivers", |v| is_known(v, 2, 0));
}

#[test]
fn a_task_driver_that_dies_is_replaced_and_the_other_drivers_stay() {
    let link = FakeLink::ready();
    let api = known_api(0, 0);
    link.queue_api(api.clone());
    let clock = Arc::new(ManualClock::new(super::rig::T0));
    // The ticker of a task driver dies at its first sleep, as soon as it starts.
    let dying = killer(&clock, &["task-tick"]);
    dying.kill.store(true, Ordering::SeqCst);
    let rig = Rig::start_on(link.clone(), clock.clone(), dying.clone(), None);
    for id in ["s1", "s2"] {
        rig.backend().open_task(id).unwrap();
    }
    // The backend finds them dead and stops them (the only way a task driver
    // is stopped here): the joined threads of a driver are counted.
    rig.wait_ticking("the dead drivers were stopped", |b| {
        b.thread_report().joined >= 1
    });
    dying.kill.store(false, Ordering::SeqCst);
    rig.wait_ticking("replaced by drivers that run", |b| {
        ["s1", "s2"].iter().all(|id| {
            let view = b.task_view(id);
            view.is_some_and(|v| v.link != crate::driver::task::LinkState::Stopped)
        })
    });
    assert_eq!(link.api_calls(), 1, "the serve's drivers were not touched");
    assert_eq!(rig.backend().with_skills(|h| h.is_running()), Some(true));
    assert_eq!(rig.backend().with_approvals(|h| h.is_running()), Some(true));
    assert_eq!(rig.view().open_tasks, ["s1", "s2"]);
}

#[test]
fn a_task_closed_is_gone_for_the_ui_before_the_worker_has_stopped_it() {
    let link = FakeLink::ready();
    link.queue_api(known_api(0, 0));
    let armed = Arc::new(AtomicBool::new(false));
    let entered = Arc::new(AtomicBool::new(false));
    let (release, gate) = mpsc::channel::<()>();
    let gate = std::sync::Mutex::new(gate);
    let stuck: OnChange = {
        let (armed, entered) = (Arc::clone(&armed), Arc::clone(&entered));
        Arc::new(move || {
            if armed.load(Ordering::SeqCst) {
                entered.store(true, Ordering::SeqCst);
                let _ = gate.lock().unwrap().recv();
            }
        })
    };
    let clock = Arc::new(ManualClock::new(super::rig::T0));
    let rig = Rig::start_on(link.clone(), clock.clone(), clock, Some(stuck));
    rig.backend().open_task("s1").unwrap();
    rig.wait_ticking("the driver", |b| b.task_view("s1").is_some());
    // Park the worker inside the UI's callback, with a change to report.
    armed.store(true, Ordering::SeqCst);
    rig.backend().open_task("s2").unwrap();
    rig.wait_ticking("the worker is in the callback", |_| {
        entered.load(Ordering::SeqCst)
    });
    // The driver of s1 is still there, but the app closed it.
    let b = rig.backend();
    assert!(b.close_task("s1"));
    assert!(b.task_view("s1").is_none());
    assert!(b.with_task("s1", |_| ()).is_none());
    assert_eq!(b.send("s1", "x"), Err(TaskRefused::NotOpen));
    assert_eq!(b.snapshot().open_tasks, ["s2"]);
    drop(release);
    rig.wait_ticking("s2 has its driver", |b| b.task_view("s2").is_some());
}
