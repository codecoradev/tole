//! The end of the driver's life, end to end: shutdown (clean, and during a
//! call), a dropped driver, and a thread that dies.

use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use tole_desktop_model::{Badge, Load};

use super::fixtures::{skills, tools, T0};
use super::rig::{config, count, known, Rig, PATIENCE};
use super::SkillsDriver;
use crate::driver::clock::{Clock, ManualClock, Shutdown, Slept};
use crate::driver::fake::FakeApi;

#[test]
fn shutdown_stops_every_thread_and_leaves_the_page_not_loaded() {
    let (_api, mut rig) = known(0);
    let handle = rig.handle.clone();
    let report = rig
        .driver
        .take()
        .expect("running")
        .shutdown(Duration::from_secs(10));
    assert_eq!((report.joined, report.panicked, report.detached), (4, 0, 0));
    assert!(!handle.is_running());
    let v = handle.snapshot();
    assert_eq!(v.skills_waiting, Badge::Unknown, "not a stale count");
    assert_eq!(v.tools_needing_attention, Badge::Unknown);
    assert_eq!(v.page.skills_load, Load::Idle);
    assert!(v.page.skills.review.is_empty() && v.page.tools.is_empty());
    assert!(handle.refresh().is_err(), "nobody to carry it out");
}

#[test]
fn shutdown_does_not_wait_for_a_call_and_its_answer_changes_nothing() {
    let api = FakeApi::new();
    let held = api.gate_skills();
    api.reply_tools(Ok(tools(1)));
    let mut rig = Rig::start(api.clone());
    api.wait_skills(1);
    rig.until("the tools are known", |v| {
        v.tools_needing_attention == Badge::Count(1)
    });
    // The skills loader sits in a call the client cannot cancel. The bound is
    // generous so that a loaded machine does not make the others late.
    let report = rig
        .driver
        .take()
        .expect("running")
        .shutdown(Duration::from_secs(2));
    assert_eq!(report.detached, 1);
    assert_eq!(report.joined, 3);
    // It ends by itself when the call returns, and the page stays empty.
    held.send(Ok(skills(9))).unwrap();
    let v = rig.handle.snapshot();
    assert_eq!(v.skills_waiting, Badge::Unknown);
    assert_eq!(v.tools_needing_attention, Badge::Unknown);
    assert!(v.page.tools.is_empty());
}

#[test]
fn dropping_the_driver_stops_it() {
    let (_api, mut rig) = known(0);
    let handle = rig.handle.clone();
    drop(rig.driver.take());
    let end = Instant::now() + PATIENCE;
    while handle.is_running() && Instant::now() < end {
        std::thread::yield_now();
    }
    assert!(!handle.is_running());
    assert_eq!(handle.snapshot().skills_waiting, Badge::Unknown);
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
fn a_thread_that_dies_outside_a_call_ends_the_driver_and_the_page_is_not_loaded() {
    let api = FakeApi::new();
    api.reply_skills(Ok(skills(2)));
    api.reply_tools(Ok(tools(1)));
    let changes = Arc::new((Mutex::new(0u64), Condvar::new()));
    let cfg = config(&changes);
    let clock = Arc::new(PanickySleep(ManualClock::new(T0)));
    let driver = SkillsDriver::start(api, clock, cfg).expect("start");
    let handle = driver.handle();
    let (n, wake) = &*changes;
    let guard = n.lock().unwrap();
    let (_g, out) = wake
        .wait_timeout_while(guard, PATIENCE, |_| handle.is_running())
        .unwrap();
    assert!(!out.timed_out() || !handle.is_running(), "the driver ended");
    assert!(!handle.is_running());
    let v = handle.snapshot();
    assert!(!count(&v, 2, 1) && v.skills_waiting == Badge::Unknown);
    assert_eq!(v.page.skills_load, Load::Idle);
}
