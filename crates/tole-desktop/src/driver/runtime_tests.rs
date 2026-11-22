//! `Spawner`: threads started from other threads are all accounted for.

use std::sync::mpsc::channel;
use std::time::Duration;

use super::runtime::{JoinReport, Threads};

#[test]
fn threads_started_one_after_another_are_all_counted_though_some_ended_before_the_next() {
    let mut threads = Threads::new();
    let spawner = threads.spawner();
    for n in 0..50 {
        let (done, ended) = channel();
        spawner
            .spawn("short", move || {
                let _ = done.send(());
                assert_ne!(n, 7, "a thread that panics");
            })
            .unwrap();
        ended.recv().unwrap();
    }
    let report = threads.join(Duration::from_secs(5));
    assert_eq!(
        report,
        JoinReport {
            joined: 50,
            panicked: 1,
            detached: 0
        }
    );
    // Counted once: nothing is left for the next join.
    assert_eq!(threads.join(Duration::ZERO), JoinReport::default());
}

#[test]
fn nothing_is_started_once_the_join_has_begun() {
    let mut threads = Threads::new();
    let spawner = threads.spawner();
    threads.join(Duration::ZERO);
    assert!(spawner.spawn("late", || {}).is_err());
}
