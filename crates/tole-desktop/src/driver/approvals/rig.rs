//! The harness of the end-to-end tests: a driver over a [`FakeApi`] and a
//! [`ManualClock`], and the waits that keep a test deterministic.

use std::sync::mpsc::Sender;
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use tole_client::StreamItem;
use tole_desktop_model::approvals::{ComposerState, Tab};
use tole_desktop_model::Badge;

use super::engine::TICK_EVERY;
use super::engine_tests::{list, row, T0};
use super::{ApprovalsDriver, ApprovalsView, Config, Handle};
use crate::driver::clock::ManualClock;
use crate::driver::fake::{FakeApi, Frame};

/// The refresh ceiling of a rig (100 hours): a test that ticks fast must not
/// reach the refresh by accident.
const CEILING: Duration = Duration::from_secs(360_000);
pub(super) const PATIENCE: Duration = Duration::from_secs(10);

pub(super) struct Rig {
    pub clock: Arc<ManualClock>,
    pub driver: Option<ApprovalsDriver>,
    pub handle: Handle,
    changes: Arc<(Mutex<u64>, Condvar)>,
}

pub(super) fn config(sessions: &[&str], seen: &Arc<(Mutex<u64>, Condvar)>) -> Config {
    let seen = Arc::clone(seen);
    Config {
        sessions: sessions.iter().map(|s| (*s).to_owned()).collect(),
        // Far away (100 hours of the monotonic clock): a test that ticks fast
        // must not reach it by accident.
        refresh: Duration::from_secs(360_000),
        on_change: Some(Arc::new(move || {
            *seen.0.lock().unwrap() += 1;
            seen.1.notify_all();
        })),
    }
}

impl Rig {
    pub(super) fn start(api: Arc<FakeApi>, sessions: &[&str]) -> Rig {
        let clock = Arc::new(ManualClock::new(T0));
        Rig::start_on(api, clock, sessions)
    }

    pub(super) fn start_on(api: Arc<FakeApi>, clock: Arc<ManualClock>, sessions: &[&str]) -> Rig {
        let changes = Arc::new((Mutex::new(0u64), Condvar::new()));
        let cfg = config(sessions, &changes);
        let driver =
            ApprovalsDriver::start_with_ceiling(api, clock.clone(), cfg, CEILING).expect("start");
        // The ticker is asleep before the test moves the time.
        assert!(clock.wait_sleeps_begun(1, PATIENCE));
        Rig {
            clock,
            handle: driver.handle(),
            driver: Some(driver),
            changes,
        }
    }

    /// Wait until the view satisfies `pred`; returns it.
    pub(super) fn until(&self, why: &str, pred: impl Fn(&ApprovalsView) -> bool) -> ApprovalsView {
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

    /// Move the clock one tick, once the ticker is asleep. Only the monotonic
    /// reading moves: a test that ticks fast must not run the model's own
    /// wall-clock deadlines (a fetch given up after 2 minutes, a request
    /// expiring) out from under a thread that is slow to run.
    pub(super) fn tick(&self) {
        let before = self.clock.sleeps_begun();
        self.clock.advance_monotonic(TICK_EVERY);
        assert!(
            self.clock.wait_sleeps_begun(before + 1, PATIENCE),
            "the ticker sleeps again"
        );
    }

    /// Returns once the worker has handled everything sent before this call:
    /// the inbox is in order, and a tab change is a visible change.
    pub(super) fn sync(&self) {
        let here = self.handle.snapshot().tab;
        let there = match here {
            Tab::Waiting => Tab::Decided,
            Tab::Decided => Tab::Waiting,
        };
        for tab in [there, here] {
            // A full inbox (a worker that is slow to run) is only a wait.
            let end = Instant::now() + PATIENCE;
            while self.handle.select_tab(tab).is_err() {
                assert!(Instant::now() < end, "the inbox stays full");
                std::thread::yield_now();
            }
            self.until("the worker caught up", |v| v.tab == tab);
        }
    }

    /// Tick (the worker handles each tick before the next) until the view
    /// satisfies `pred`. Bounded in real time, not in ticks: a thread that is
    /// slow to run (a fetch, a reader) must not make the loop give up, and a
    /// tick it sleeps through only makes the next one count.
    pub(super) fn tick_until(
        &self,
        why: &str,
        pred: impl Fn(&ApprovalsView) -> bool,
    ) -> ApprovalsView {
        let end = Instant::now() + PATIENCE;
        while Instant::now() < end {
            let v = self.handle.snapshot();
            if pred(&v) {
                return v;
            }
            self.tick();
            self.sync();
        }
        self.until(why, pred)
    }

    /// Tick until the api has seen `n` fetches.
    pub(super) fn tick_until_calls(&self, api: &FakeApi, n: usize) {
        let end = Instant::now() + PATIENCE;
        while Instant::now() < end && api.list_calls() < n {
            self.tick();
            self.sync();
        }
        api.wait_list_calls(n);
    }

    /// Tick until the api has seen `n` opens of `session`'s stream. The ticker
    /// and the reader are asleep before each tick (a reader that starts its
    /// wait after the clock moved would wait for the next tick).
    pub(super) fn tick_until_opens(&self, api: &FakeApi, session: &str, n: usize) {
        let end = Instant::now() + PATIENCE;
        while Instant::now() < end && api.opens(session) < n {
            if self.clock.wait_sleepers(2, Duration::from_secs(1)) && api.opens(session) < n {
                self.tick();
            }
        }
        api.wait_opens(session, n);
    }

    /// The stream says something: it is up.
    pub(super) fn up(stream: &Sender<Frame>) {
        stream.send(Frame::Item(StreamItem::Heartbeat)).unwrap();
    }
}

pub(super) fn counted(v: &ApprovalsView, n: usize) -> bool {
    v.badge == Badge::Count(n)
}

pub(super) fn composers(task: &str, n: usize) -> Vec<(String, ComposerState)> {
    vec![(
        task.to_owned(),
        ComposerState::WaitingForAnswer { requests: n },
    )]
}

/// A rig that follows `s1`, known after its stream came up and the fetch that
/// followed it. The script holds two lists.
pub(super) fn following(rows: Vec<serde_json::Value>) -> (Arc<FakeApi>, Rig, Sender<Frame>) {
    let api = FakeApi::new();
    api.reply_list(Ok(list(rows.clone())));
    api.reply_list(Ok(list(rows)));
    let stream = api.open_stream("s1");
    let rig = Rig::start(api.clone(), &["s1"]);
    Rig::up(&stream);
    rig.tick_until("known after the stream is up", |v| {
        v.badge != Badge::Unknown
    });
    (api, rig, stream)
}

/// Replies for fetches the test cannot count: how many run depends on whether a
/// stream came up before or after a fetch began (a fetch that began before it
/// is asked again), so a test that waits for "known" scripts plenty.
pub(super) fn spare(api: &FakeApi, rows: Vec<serde_json::Value>) {
    for _ in 0..8 {
        api.reply_list(Ok(list(rows.clone())));
    }
}

pub(super) fn pending() -> Vec<serde_json::Value> {
    vec![row("a1", "s1", "pending")]
}

pub(super) fn decide_rig() -> (Arc<FakeApi>, Rig) {
    let api = FakeApi::new();
    api.reply_list(Ok(list(pending())));
    let rig = Rig::start(api.clone(), &[]);
    rig.until("known", |v| counted(v, 1));
    (api, rig)
}
