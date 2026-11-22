//! The harness of the end-to-end tests: a driver over a [`FakeApi`] and a
//! [`ManualClock`], and the waits that keep a test deterministic.

use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use tole_desktop_model::{Badge, Tab};

use super::engine::TICK_EVERY;
use super::fixtures::{skills, tools, T0};
use super::{Config, Handle, SkillsDriver, SkillsPageView, DEFAULT_REFRESH, MAX_REFRESH};
use crate::driver::clock::ManualClock;
use crate::driver::fake::FakeApi;

pub(super) const PATIENCE: Duration = Duration::from_secs(10);

pub(super) struct Rig {
    pub clock: Arc<ManualClock>,
    pub driver: Option<SkillsDriver>,
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
        let driver =
            SkillsDriver::start_with_ceiling(api, clock.clone(), cfg, MAX_REFRESH).expect("start");
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
        pred: impl Fn(&SkillsPageView) -> bool,
    ) -> SkillsPageView {
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

    /// Returns once the worker has handled everything sent before this call:
    /// the inbox is in order, and a tab change is a visible change.
    pub(super) fn sync(&self) {
        let here = self.handle.snapshot().page.tab;
        let there = match here {
            Tab::Skills => Tab::ConnectedTools,
            Tab::ConnectedTools => Tab::Skills,
        };
        for tab in [there, here] {
            // A full inbox (a worker that is slow to run) is only a wait.
            let end = Instant::now() + PATIENCE;
            while self.handle.select_tab(tab).is_err() {
                assert!(Instant::now() < end, "the inbox stays full");
                std::thread::yield_now();
            }
            self.until("the worker caught up", |v| v.page.tab == tab);
        }
    }

    /// Move the clock one tick and wait for the api to see `n` calls of
    /// `GET /skills`. For a test that must see what a call's outcome does
    /// before more time passes (a failure, before its retry): a loop that ticks
    /// until the call is seen could run the clock past the outcome.
    pub(super) fn tick_to_skills(&self, api: &FakeApi, n: usize) {
        self.tick();
        api.wait_skills(n);
    }

    /// Tick `n` seconds, letting the worker catch up after each.
    pub(super) fn tick_for(&self, n: u32) {
        for _ in 0..n {
            self.tick();
            self.sync();
        }
    }
}

pub(super) fn count(v: &SkillsPageView, skills: usize, tools: usize) -> bool {
    v.skills_waiting == Badge::Count(skills) && v.tools_needing_attention == Badge::Count(tools)
}

/// A rig whose first calls are answered: two drafts waiting, one untrusted
/// tool. Spare replies are left for the calls the test makes later.
pub(super) fn known(spare: usize) -> (Arc<FakeApi>, Rig) {
    let api = FakeApi::new();
    api.reply_skills(Ok(skills(2)));
    api.reply_tools(Ok(tools(1)));
    for _ in 0..spare {
        api.reply_skills(Ok(skills(2)));
        api.reply_tools(Ok(tools(1)));
    }
    let rig = Rig::start(api.clone());
    rig.until("known", |v| count(v, 2, 1));
    (api, rig)
}
