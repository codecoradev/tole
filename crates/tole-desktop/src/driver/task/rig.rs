//! The harness of the end-to-end tests: a driver over a [`FakeApi`] and a
//! [`ManualClock`], and the waits that keep a test deterministic.
//!
//! Time moves only through [`Rig::tick`], one 250 ms step at a time, and each
//! step is followed by [`Rig::sync`], which returns once the worker has handled
//! everything sent before it (the inbox is in order, and a group toggle is a
//! visible change). A reply a test wants to see applied before the next step
//! is held with a gate and released when the test is ready.

use std::sync::mpsc::Sender;
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;
use std::time::Instant;

use tole_client::StreamItem;
use tole_desktop_model::task::Item;

use super::engine::TICK_EVERY;
use super::fixtures::*;
use super::{Config, Handle, TaskDriver, TaskView};
use crate::driver::clock::ManualClock;
use crate::driver::fake::{FakeApi, Frame};

pub(super) const PATIENCE: Duration = Duration::from_secs(10);

pub(super) struct Rig {
    pub api: Arc<FakeApi>,
    pub clock: Arc<ManualClock>,
    pub driver: Option<TaskDriver>,
    pub handle: Handle,
    changes: Arc<(Mutex<u64>, Condvar)>,
}

impl Rig {
    pub(super) fn start(api: Arc<FakeApi>, mut cfg: Config) -> Rig {
        let changes = Arc::new((Mutex::new(0u64), Condvar::new()));
        let seen = Arc::clone(&changes);
        cfg.on_change = Some(Arc::new(move || {
            *seen.0.lock().unwrap() += 1;
            seen.1.notify_all();
        }));
        let rig = Rig::start_with(api, cfg, Arc::clone(&changes));
        // The worker tells the UI at its first step: its clock readings (the
        // time a first read is due from) are taken before the test moves it.
        let (count, wake) = &*changes;
        let started = wake
            .wait_timeout_while(count.lock().unwrap(), PATIENCE, |n| *n == 0)
            .unwrap();
        assert!(!started.1.timed_out(), "the worker started");
        drop(started);
        rig
    }

    /// Start with the callback the config already has (a test that blocks it).
    pub(super) fn start_with(
        api: Arc<FakeApi>,
        cfg: Config,
        changes: Arc<(Mutex<u64>, Condvar)>,
    ) -> Rig {
        let clock = Arc::new(ManualClock::new(super::sim::T0));
        let driver = TaskDriver::start(api.clone(), clock.clone(), cfg).expect("start");
        // The ticker is asleep before the test moves the time.
        assert!(clock.wait_sleeps_begun(1, PATIENCE));
        Rig {
            api,
            clock,
            handle: driver.handle(),
            driver: Some(driver),
            changes,
        }
    }

    /// Wait until the view satisfies `pred`; returns it.
    pub(super) fn until(&self, why: &str, pred: impl Fn(&TaskView) -> bool) -> TaskView {
        let (count, wake) = &*self.changes;
        loop {
            let seen = *count.lock().unwrap();
            let v = self.handle.snapshot();
            if pred(&v) {
                return v;
            }
            let guard = count.lock().unwrap();
            let (_guard, out) = wake
                .wait_timeout_while(guard, PATIENCE, |c| *c == seen)
                .unwrap();
            assert!(
                !out.timed_out(),
                "timed out waiting for: {why}; view: {v:?}"
            );
        }
    }

    /// Returns once the worker has handled everything sent before this call:
    /// the inbox is in order, and flipping the first row's group (the rigs
    /// start with a row that is one) is a change a view can see.
    pub(super) fn sync(&self) {
        let open = |v: &TaskView| matches!(v.rows.first(), Some(Item::Work(g)) if g.expanded);
        let before = open(&self.handle.snapshot());
        // A full inbox (a worker that is slow to run) is only a wait.
        let end = Instant::now() + PATIENCE;
        while self.handle.toggle_group(0).is_err() {
            assert!(Instant::now() < end, "the inbox stays full");
            std::thread::yield_now();
        }
        self.until("the worker caught up", |v| open(v) != before);
    }

    /// Move the monotonic clock by `d` (the wall clock stands still: a suspend
    /// is a test of its own) and wait until the worker has handled the tick.
    /// Calls that fall due are then waited for with `wait_*` on the api, and a
    /// reply that is applied late changes nothing: the next poll is due a wait
    /// after the last one was made, and a reply is itself a step.
    pub(super) fn advance(&self, d: Duration) {
        let before = self.clock.sleeps_begun();
        self.clock.advance_monotonic(d);
        assert!(
            self.clock.wait_sleeps_begun(before + 1, PATIENCE),
            "the ticker sleeps again"
        );
        self.sync();
    }

    /// Move on in steps of `d` until the api has seen `n` status calls: for a
    /// watch whose start a test cannot see. Only for a watch that has no
    /// timeout of its own to run out meanwhile.
    pub(super) fn advance_until_status(&self, n: usize) {
        let end = Instant::now() + PATIENCE;
        while self.api.status_calls() < n {
            assert!(Instant::now() < end, "timed out waiting for status calls");
            self.advance(Duration::from_millis(500));
        }
    }

    /// One tick.
    pub(super) fn tick(&self) {
        self.advance(TICK_EVERY);
    }

    pub(super) fn up(stream: &Sender<Frame>) {
        stream.send(Frame::Item(StreamItem::Heartbeat)).unwrap();
    }

    pub(super) fn feed(stream: &Sender<Frame>, item: StreamItem) {
        stream.send(Frame::Item(item)).unwrap();
    }
}

/// A rig on a session with no history, its stream up and queue read (empty).
pub(super) fn fresh() -> (Arc<FakeApi>, Rig, Sender<Frame>) {
    let api = FakeApi::new();
    api.reply_list(Ok(list(vec![])));
    let stream = api.open_stream(S);
    let mut cfg = Config::new(S);
    cfg.fresh = true;
    let rig = Rig::start(api.clone(), cfg);
    Rig::up(&stream);
    api.wait_list_calls(1);
    // One finished tool call: a row whose group the tests flip to sync.
    Rig::feed(&stream, entry_item(&intent(1, "read_file")));
    Rig::feed(&stream, entry_item(&settled(2, 1)));
    rig.until("live with a row", |v| {
        v.link == super::LinkState::Live && v.total_rows == 1
    });
    (api, rig, stream)
}
