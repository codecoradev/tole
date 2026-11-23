//! The harness of the end-to-end tests: a driver over a [`FakeApi`] and a
//! [`ManualClock`], and the waits that keep a test deterministic.

use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use super::engine::TICK_EVERY;
use super::fixtures::{jobs_reply, three, T0};
use super::{AutomationsDriver, AutomationsView, Config, Handle, DEFAULT_REFRESH, MAX_REFRESH};
use crate::driver::clock::ManualClock;
use crate::driver::fake::FakeApi;

pub(super) const PATIENCE: Duration = Duration::from_secs(10);

pub(super) struct Rig {
    pub clock: Arc<ManualClock>,
    pub driver: Option<AutomationsDriver>,
    pub handle: Handle,
    changes: Arc<(Mutex<u64>, Condvar)>,
}

pub(super) fn config(seen: &Arc<(Mutex<u64>, Condvar)>) -> Config {
    let seen = Arc::clone(seen);
    Config {
        refresh: DEFAULT_REFRESH,
        on_change: Some(Arc::new(move || {
            *seen.0.lock().unwrap() += 1;
            seen.1.notify_all();
        })),
    }
}

impl Rig {
    pub(super) fn start(api: Arc<FakeApi>) -> Rig {
        let changes = Arc::new((Mutex::new(0u64), Condvar::new()));
        let cfg = config(&changes);
        Rig::start_with(api, cfg, changes)
    }

    pub(super) fn start_with(
        api: Arc<FakeApi>,
        cfg: Config,
        changes: Arc<(Mutex<u64>, Condvar)>,
    ) -> Rig {
        let clock = Arc::new(ManualClock::new(T0));
        let driver = AutomationsDriver::start_with_ceiling(api, clock.clone(), cfg, MAX_REFRESH)
            .expect("start");
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
    pub(super) fn until(
        &self,
        why: &str,
        pred: impl Fn(&AutomationsView) -> bool,
    ) -> AutomationsView {
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

    /// Move the monotonic clock by `d` once the ticker is asleep, and wait for
    /// it to sleep again. The wall clock stands still.
    pub(super) fn advance(&self, d: Duration) {
        let before = self.clock.sleeps_begun();
        self.clock.advance_monotonic(d);
        assert!(
            self.clock.wait_sleeps_begun(before + 1, PATIENCE),
            "the ticker sleeps again"
        );
    }

    pub(super) fn tick(&self) {
        self.advance(TICK_EVERY);
    }

    /// Retry a command until the inbox has room for it.
    pub(super) fn send(&self, command: impl Fn(&Handle) -> Result<(), super::Busy>) {
        let end = Instant::now() + PATIENCE;
        while command(&self.handle).is_err() {
            assert!(Instant::now() < end, "the inbox stays full");
            std::thread::yield_now();
        }
    }

    /// Returns once the worker has handled everything sent before this call:
    /// the inbox is in order, and a press on a job that does not exist leaves a
    /// notice that is a visible change. Needs a page with no notice.
    pub(super) fn sync(&self) {
        assert!(
            self.handle.snapshot().page.notice.is_none(),
            "sync needs a page with no notice"
        );
        self.send(|h| h.set_enabled("sync-no-such-job", true));
        self.until("the worker caught up", |v| v.page.notice.is_some());
        self.send(Handle::dismiss_notice);
        self.until("the notice is gone", |v| v.page.notice.is_none());
    }

    /// Tick `n` seconds, letting the worker catch up after each.
    pub(super) fn tick_for(&self, n: u32) {
        for _ in 0..n {
            self.tick();
            self.sync();
        }
    }
}

/// A rig whose first call is answered: `on`, `off` and `tole`. Spare replies
/// are left for the calls the test makes later.
pub(super) fn known(spare: usize) -> (Arc<FakeApi>, Rig) {
    let api = FakeApi::new();
    api.reply_cron_jobs(jobs_reply(three()));
    for _ in 0..spare {
        api.reply_cron_jobs(jobs_reply(three()));
    }
    let rig = Rig::start(api.clone());
    rig.until("known", |v| v.counts() == Some((1, 2)));
    (api, rig)
}
