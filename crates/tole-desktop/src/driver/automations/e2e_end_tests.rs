//! The end of the driver's life and the calls that go wrong, end to end:
//! shutdown (clean, and during a call), a dropped driver, a thread that dies and
//! a panic inside a call.

use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use tole_desktop_model::{AutomationId, Load, Notice, Switch};

use super::fixtures::*;
use super::rig::{config, known, Rig, PATIENCE};
use super::AutomationsDriver;
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
    assert_eq!((report.joined, report.panicked, report.detached), (5, 0, 0));
    assert!(!handle.is_running());
    let v = handle.snapshot();
    assert!(
        v.page.rows.is_empty() && v.counts().is_none(),
        "not a stale list"
    );
    assert_eq!(v.page.load, Load::Idle);
    assert!(handle.refresh().is_err(), "nobody to carry it out");
    assert!(handle.set_enabled("on", false).is_err());
    assert!(handle.select("on").is_err());
}

#[test]
fn shutdown_does_not_wait_for_a_call_and_its_answer_changes_nothing() {
    let api = FakeApi::new();
    let held = api.gate_cron_jobs();
    let mut rig = Rig::start(api.clone());
    api.wait_cron_jobs(1);
    // The loader sits in a call the client cannot cancel. The bound is
    // generous so that a loaded machine does not make the others late.
    let report = rig
        .driver
        .take()
        .expect("running")
        .shutdown(Duration::from_secs(2));
    assert_eq!((report.detached, report.joined), (1, 4));
    held.send(jobs_reply(three())).unwrap();
    let v = rig.handle.snapshot();
    assert!(v.page.rows.is_empty() && v.counts().is_none());
}

#[test]
fn a_switch_in_flight_at_shutdown_is_not_applied_afterwards() {
    let (api, mut rig) = known(0);
    let held = api.gate_pause();
    rig.handle.set_enabled("on", false).unwrap();
    api.wait_pauses(1);
    let handle = rig.handle.clone();
    let report = rig
        .driver
        .take()
        .expect("running")
        .shutdown(Duration::from_secs(2));
    assert_eq!(report.detached, 1);
    held.send(toggled_to("on", "paused")).unwrap();
    assert!(handle.snapshot().page.rows.is_empty());
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
    assert!(handle.snapshot().page.rows.is_empty());
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
    api.reply_cron_jobs(jobs_reply(three()));
    let changes = Arc::new((Mutex::new(0u64), Condvar::new()));
    let cfg = config(&changes);
    let clock = Arc::new(PanickySleep(ManualClock::new(T0)));
    let driver = AutomationsDriver::start(api, clock, cfg).expect("start");
    let handle = driver.handle();
    let (n, wake) = &*changes;
    let guard = n.lock().unwrap();
    let (_g, out) = wake
        .wait_timeout_while(guard, PATIENCE, |_| handle.is_running())
        .unwrap();
    assert!(!out.timed_out() || !handle.is_running(), "the driver ended");
    assert!(!handle.is_running());
    let v = handle.snapshot();
    assert!(v.page.rows.is_empty() && v.counts().is_none());
    assert_eq!(v.page.load, Load::Idle);
}

#[test]
fn a_panic_in_the_list_call_is_a_failed_read_and_it_is_retried() {
    let api = FakeApi::new();
    api.panic_cron_jobs();
    api.reply_cron_jobs(jobs_reply(three()));
    let rig = Rig::start(api.clone());
    rig.until("failed", |v| matches!(v.page.load, Load::Failed(_)));
    assert!(rig.handle.is_running());
    rig.tick();
    rig.until("known", |v| v.counts() == Some((1, 2)));
}

#[test]
fn a_panic_in_a_switch_call_is_not_reaching_tole_and_the_next_press_works() {
    let (api, rig) = known(2);
    api.panic_pause();
    rig.handle.set_enabled("on", false).unwrap();
    let v = rig.until("a notice", |v| v.page.notice.is_some());
    assert_eq!(v.page.notice, Some(Notice::NotReached));
    assert_eq!(v.toggling, None);
    assert_eq!(
        v.page.row(&AutomationId::new("on")).map(|r| r.switch),
        Some(Switch::On)
    );
    assert!(rig.handle.is_running());
    api.reply_pause(toggled_to("on", "paused"));
    rig.handle.set_enabled("on", false).unwrap();
    rig.until("paused", |v| {
        v.page.row(&AutomationId::new("on")).map(|r| r.switch) == Some(Switch::Off)
    });
    assert_eq!(api.pauses(), ["on", "on"], "the second is a new press");
}

#[test]
fn a_panic_in_the_history_call_is_a_failed_history_and_it_is_retried() {
    let (api, rig) = known(0);
    api.panic_cron_runs();
    api.reply_cron_runs(Ok(runs_of("on", &["ok"])));
    rig.handle.select("on").unwrap();
    rig.until("failed", |v| matches!(v.history.load, Load::Failed(_)));
    rig.tick();
    rig.until("history", |v| v.history.load == Load::Ready);
}
