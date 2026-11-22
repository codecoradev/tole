//! The foundation on its own: the stop signal, the clocks and the threads.

use std::sync::mpsc::channel;
use std::sync::Arc;
use std::time::Duration;

use super::clock::{Clock, ManualClock, Shutdown, Slept, SystemClock};
use super::runtime::{JoinReport, Threads};

const LONG: Duration = Duration::from_secs(3600);

#[test]
fn a_shutdown_stays_raised_and_wakes_a_sleeper_on_either_clock() {
    let stop = Shutdown::new();
    assert!(!stop.is_set());
    let manual = Arc::new(ManualClock::new(0));
    let sleepers = [
        Arc::clone(&manual) as Arc<dyn Clock>,
        Arc::new(SystemClock) as Arc<dyn Clock>,
    ];
    let threads: Vec<_> = sleepers
        .into_iter()
        .map(|clock| {
            let stop = stop.clone();
            std::thread::spawn(move || clock.sleep(LONG, &stop))
        })
        .collect();
    // The manual sleeper has begun; the system one begins on its own time and
    // sees the raised signal either way.
    assert!(manual.wait_sleeps_begun(1, Duration::from_secs(10)));
    stop.trigger();
    for t in threads {
        assert_eq!(t.join().unwrap(), Slept::Stopped);
    }
    assert!(stop.is_set());
    // Raised for good: a sleep that begins now returns at once.
    assert_eq!(SystemClock.sleep(LONG, &stop), Slept::Stopped);
    assert_eq!(manual.sleep(LONG, &stop), Slept::Stopped);
}

#[test]
fn the_system_clock_reads_a_sane_wall_time_and_a_monotonic_one_that_does_not_step_back() {
    let c = SystemClock;
    assert!(c.now_ms() > 1_600_000_000_000, "after 2020");
    let a = c.monotonic_ms();
    let b = c.monotonic_ms();
    assert!(b >= a);
    assert_eq!(c.sleep(Duration::ZERO, &Shutdown::new()), Slept::Elapsed);
}

#[test]
fn the_manual_clock_moves_only_when_told_and_wakes_a_sleeper_at_its_deadline() {
    let clock = Arc::new(ManualClock::new(1_000));
    assert_eq!((clock.now_ms(), clock.monotonic_ms()), (1_000, 0));
    let stop = Shutdown::new();
    let sleeper = {
        let (clock, stop) = (Arc::clone(&clock), stop.clone());
        std::thread::spawn(move || clock.sleep(Duration::from_secs(10), &stop))
    };
    assert!(clock.wait_sleeps_begun(1, Duration::from_secs(10)));
    clock.advance(Duration::from_secs(9));
    // Not yet: it is still asleep (it would have ended by now if it could).
    assert_eq!(clock.sleeps_begun(), 1);
    assert_eq!((clock.now_ms(), clock.monotonic_ms()), (10_000, 9_000));
    clock.advance(Duration::from_secs(1));
    assert_eq!(sleeper.join().unwrap(), Slept::Elapsed);
    assert!(!stop.is_set());
}

#[test]
fn the_wall_clock_can_step_and_the_monotonic_one_does_not_follow() {
    let clock = ManualClock::new(5_000_000);
    clock.advance(Duration::from_secs(1));
    clock.set_wall_ms(1_000); // back
    assert_eq!((clock.now_ms(), clock.monotonic_ms()), (1_000, 1_000));
    clock.set_wall_ms(u64::MAX); // forward, to the end
    clock.advance(Duration::from_secs(1));
    assert_eq!((clock.now_ms(), clock.monotonic_ms()), (u64::MAX, 2_000));
}

#[test]
fn threads_are_joined_and_a_panic_and_a_blocked_thread_are_reported() {
    let mut threads = Threads::new();
    let (hold_tx, hold_rx) = channel::<()>();
    threads.spawn("quiet", || {}).unwrap();
    threads
        .spawn("panics", || panic!("a thread that panics"))
        .unwrap();
    threads
        .spawn("blocked", move || {
            let _ = hold_rx.recv();
        })
        .unwrap();
    let report = threads.join(Duration::from_millis(500));
    assert_eq!(
        report,
        JoinReport {
            joined: 2,
            panicked: 1,
            detached: 1
        }
    );
    drop(hold_tx); // the blocked one ends by itself
                   // Nothing left to join.
    assert_eq!(threads.join(Duration::ZERO), JoinReport::default());
}

#[test]
fn a_join_returns_as_soon_as_every_thread_has_ended() {
    let mut threads = Threads::new();
    for _ in 0..4 {
        threads.spawn("quick", || {}).unwrap();
    }
    // A generous bound that is never reached: the exits wake the join.
    let report = threads.join(Duration::from_secs(60));
    assert_eq!((report.joined, report.detached), (4, 0));
}

#[test]
fn an_advance_returns_only_after_the_sleeps_that_fell_due_have_ended() {
    let clock = Arc::new(ManualClock::new(0));
    let stop = Shutdown::new();
    let (due, later) = (
        {
            let (clock, stop) = (Arc::clone(&clock), stop.clone());
            std::thread::spawn(move || clock.sleep(Duration::from_secs(1), &stop))
        },
        {
            let (clock, stop) = (Arc::clone(&clock), stop.clone());
            std::thread::spawn(move || clock.sleep(Duration::from_secs(100), &stop))
        },
    );
    assert!(clock.wait_sleepers(2, Duration::from_secs(10)));
    clock.advance(Duration::from_secs(2));
    // The one that fell due is already gone when `advance` returns; the other
    // is still asleep.
    assert!(clock.wait_sleepers(1, Duration::ZERO));
    assert!(!clock.wait_sleepers(2, Duration::ZERO));
    assert_eq!(due.join().unwrap(), Slept::Elapsed);
    stop.trigger();
    assert_eq!(later.join().unwrap(), Slept::Stopped);
}
