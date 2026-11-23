//! The small pieces on their own: stopping drivers together, the change flag the
//! drivers share, the open set, and the session id rule.

use std::sync::mpsc::{sync_channel, TryRecvError};
use std::time::{Duration, Instant};

use super::reap::{self, Job};
use super::shared::{Fanin, Msg};
use super::tasks::TaskSet;
use super::OpenRefused;
use crate::driver::runtime::JoinReport;
use crate::driver::shared::ids::{is_session_id, MAX_SESSION_ID_LEN};
use crate::driver::task::MAX_OPEN_TASKS;

fn report(joined: usize, panicked: usize, detached: usize) -> JoinReport {
    JoinReport {
        joined,
        panicked,
        detached,
    }
}

#[test]
fn drivers_are_stopped_all_at_once_not_one_after_the_other() {
    let jobs: Vec<Job> = (0..4)
        .map(|_| -> Job {
            Box::new(|| {
                std::thread::sleep(Duration::from_millis(200));
                report(2, 0, 1)
            })
        })
        .collect();
    let begun = Instant::now();
    let total = reap::all(jobs);
    assert_eq!(total, report(8, 0, 4), "the reports are added up");
    assert!(
        begun.elapsed() < Duration::from_millis(600),
        "four waits of 200 ms in {:?}",
        begun.elapsed()
    );
}

#[test]
fn a_shutdown_that_panics_is_counted_and_the_others_still_run() {
    let jobs: Vec<Job> = vec![
        Box::new(|| report(1, 0, 0)),
        Box::new(|| panic!("a driver's shutdown panics")),
        Box::new(|| report(1, 0, 0)),
    ];
    let total = reap::all(jobs);
    assert_eq!(total, report(2, 1, 0));
}

#[test]
fn no_drivers_is_an_empty_report() {
    assert_eq!(reap::all(Vec::new()), JoinReport::default());
}

#[test]
fn a_thousand_changes_wake_the_worker_once() {
    let (inbox, rx) = sync_channel::<Msg>(4);
    let fan = Fanin::new(inbox);
    let callback = fan.callback();
    for _ in 0..1_000 {
        callback();
    }
    assert!(matches!(rx.try_recv(), Ok(Msg::Wake)));
    assert!(
        matches!(rx.try_recv(), Err(TryRecvError::Empty)),
        "one wake"
    );
    assert!(fan.is_pending());
    assert!(fan.take(), "one pending change");
    assert!(!fan.take(), "and only one");
    callback();
    assert!(
        matches!(rx.try_recv(), Ok(Msg::Wake)),
        "a new change wakes again"
    );
}

#[test]
fn a_change_is_not_lost_when_the_inbox_is_full() {
    let (inbox, rx) = sync_channel::<Msg>(1);
    inbox.send(Msg::Tick).unwrap(); // full
    let fan = Fanin::new(inbox);
    fan.mark();
    assert!(fan.is_pending(), "the flag is what counts, not the wake");
    assert!(matches!(rx.try_recv(), Ok(Msg::Tick)));
    assert!(matches!(rx.try_recv(), Err(TryRecvError::Empty)));
    assert!(fan.take());
}

#[test]
fn a_change_after_the_worker_is_gone_does_not_panic() {
    let (inbox, rx) = sync_channel::<Msg>(1);
    let fan = Fanin::new(inbox);
    drop(rx);
    fan.mark();
    assert!(fan.take());
}

#[test]
fn a_session_id_is_one_to_sixty_four_letters_digits_dashes_and_underscores() {
    let good = [
        "a",
        "A9",
        "a-b_c",
        "20261123-1",
        &"x".repeat(MAX_SESSION_ID_LEN),
    ];
    for id in good {
        assert!(is_session_id(id), "{id}");
    }
    let bad = [
        "",
        " ",
        "a b",
        "a/b",
        "a.b",
        "a\u{0}b",
        "a\nb",
        "é",
        "日本",
        &"x".repeat(MAX_SESSION_ID_LEN + 1),
        &"x".repeat(1 << 22),
    ];
    for id in bad {
        assert!(
            !is_session_id(id),
            "{:?}",
            id.chars().take(8).collect::<String>()
        );
    }
}

#[test]
fn the_open_set_keeps_the_order_the_cap_and_the_ids_it_was_given() {
    let set = TaskSet::default();
    assert_eq!(set.open("b"), Ok(true));
    assert_eq!(set.open("a"), Ok(true));
    assert_eq!(set.open("b"), Ok(false), "open already");
    assert_eq!(set.open("c"), Ok(true));
    assert_eq!(set.list(), ["b", "a", "c"]);
    assert_eq!(
        set.open("d"),
        Err(OpenRefused::TooMany {
            max: MAX_OPEN_TASKS
        })
    );
    assert_eq!(set.open("b"), Ok(false), "an open one is fine at the cap");
    assert_eq!(
        set.open("a b"),
        Err(OpenRefused::InvalidId),
        "id before cap"
    );
    assert!(set.close("a"));
    assert!(!set.close("a"));
    assert!(set.contains("b") && !set.contains("a"));
    assert_eq!(set.open("d"), Ok(true));
    assert_eq!(set.list(), ["b", "c", "d"]);
}
