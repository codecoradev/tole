//! The end of the driver's life and its panics, end to end: shutdown, a thread
//! that dies, a call that panics.

use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use tole_desktop_model::approvals::{CardState, ComposerState, Decision};
use tole_desktop_model::Badge;

use super::engine_tests::{list, reply, T0};
use super::rig::{config, counted, decide_rig, following, pending, spare, Rig, PATIENCE};
use super::ApprovalsDriver;
use crate::driver::clock::{Clock, ManualClock, Shutdown, Slept};
use crate::driver::fake::{FakeApi, Frame};

#[test]
fn shutdown_stops_every_thread_and_leaves_the_view_unknown() {
    let api = FakeApi::new();
    api.reply_list(Ok(list(pending())));
    let mut rig = Rig::start(api.clone(), &[]);
    rig.until("known", |v| counted(v, 1));
    let handle = rig.handle.clone();
    let report = rig
        .driver
        .take()
        .expect("running")
        .shutdown(Duration::from_secs(10));
    assert_eq!((report.joined, report.panicked, report.detached), (4, 0, 0));
    assert!(!handle.is_running());
    assert_eq!(handle.snapshot().badge, Badge::Unknown, "not a stale count");
    assert_eq!(handle.composer("s1"), ComposerState::Checking);
    assert!(
        handle.decide("a1", Decision::AllowOnce).is_err(),
        "nobody to carry it out"
    );
}

#[test]
fn shutdown_does_not_wait_for_a_blocked_stream_and_says_so() {
    let api = FakeApi::new();
    api.reply_list(Ok(list(vec![])));
    let held = api.open_stream("s1");
    let mut rig = Rig::start(api.clone(), &["s1"]);
    api.wait_opens("s1", 1);
    // The four other threads end at once; the reader sits in a read. The bound
    // is generous so that a loaded machine does not make the others late.
    let report = rig
        .driver
        .take()
        .expect("running")
        .shutdown(Duration::from_secs(2));
    assert_eq!(
        report.detached, 1,
        "the stream is inside a read the client cannot cancel"
    );
    assert_eq!(report.joined, 4);
    // It ends by itself when the read returns, and sends nothing.
    drop(held);
    assert_eq!(rig.handle.snapshot().badge, Badge::Unknown);
}

// ---- panics ----

#[test]
fn a_panic_in_a_decision_call_is_that_decisions_failure_and_the_driver_goes_on() {
    let (api, rig) = decide_rig();
    api.panic_decision();
    rig.handle.decide("a1", Decision::AllowOnce).unwrap();
    api.wait_decisions(1);
    // The card does not stay "answering" for ever: it waits again.
    rig.until("waiting again", |v| {
        v.selected
            .as_ref()
            .is_some_and(|c| c.state == CardState::Waiting)
    });
    assert!(rig.handle.is_running());
    // And the owner can answer: the decision call is not wedged.
    api.reply_decision(Ok(reply("a1", "approved")));
    rig.handle.decide("a1", Decision::AllowOnce).unwrap();
    rig.until("decided", |v| counted(v, 0));
}

#[test]
fn a_panic_in_the_fetch_call_is_a_failed_fetch_and_it_is_retried() {
    let api = FakeApi::new();
    api.panic_list();
    api.reply_list(Ok(list(pending())));
    let rig = Rig::start(api.clone(), &[]);
    api.wait_list_calls(1);
    rig.until("a failure is counted", |v| v.fetch_failures == 1);
    assert_eq!(rig.handle.snapshot().badge, Badge::Unknown);
    rig.tick_until("known after the retry", |v| counted(v, 1));
    assert!(rig.handle.is_running());
}

#[test]
fn a_panic_in_a_stream_is_a_lost_stream_that_is_opened_again() {
    let (api, rig, stream) = following(pending());
    let again = api.open_stream("s1");
    // One fetch follows the loss at once, one the stream coming back.
    spare(&api, pending());
    stream.send(Frame::Panic).unwrap();
    rig.until("lost", |v| v.badge == Badge::Unknown);
    rig.tick_until_opens(&api, "s1", 2);
    Rig::up(&again);
    rig.tick_until("known again", |v| counted(v, 1));
    assert!(rig.handle.is_running());
}

/// A clock whose `sleep` panics: the ticker thread dies outside any `Api` call.
struct PanickySleep(ManualClock);

impl Clock for PanickySleep {
    fn now_ms(&self) -> u64 {
        self.0.now_ms()
    }
    fn monotonic_ms(&self) -> u64 {
        self.0.monotonic_ms()
    }
    fn sleep(&self, _d: Duration, _stop: &Shutdown) -> Slept {
        panic!("a thread dies outside any call");
    }
}

#[test]
fn a_thread_that_dies_outside_a_call_ends_the_driver_and_the_view_is_unknown() {
    let api = FakeApi::new();
    api.reply_list(Ok(list(pending())));
    let changes = Arc::new((Mutex::new(0u64), Condvar::new()));
    let cfg = config(&[], &changes);
    let clock = Arc::new(PanickySleep(ManualClock::new(T0)));
    let driver = ApprovalsDriver::start(api, clock, cfg).expect("start");
    let handle = driver.handle();
    let (count, wake) = &*changes;
    let guard = count.lock().unwrap();
    let (_g, out) = wake
        .wait_timeout_while(guard, PATIENCE, |_| handle.is_running())
        .unwrap();
    assert!(!out.timed_out() || !handle.is_running(), "the driver ended");
    assert!(!handle.is_running());
    assert_eq!(handle.snapshot().badge, Badge::Unknown);
    assert_eq!(handle.composer("s1"), ComposerState::Checking);
}
