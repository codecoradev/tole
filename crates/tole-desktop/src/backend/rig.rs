//! The harness of the end-to-end tests: a backend over a [`FakeLink`] and a
//! [`ManualClock`], and the waits that keep a test deterministic.
//!
//! Time moves only through [`Rig::tick`] and [`Rig::advance`], 100 ms (one
//! backend tick) at a time. The backend's own `on_change` is paced by that
//! clock (at most one call per 100 ms of it), so a test does not wait for a
//! callback to see a change: [`Rig::until`] reads the view until it satisfies a
//! condition, which is what a UI would do on the next repaint.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::json;
use tole_client::{ApprovalList, SkillsState, ToolsState};
use tole_desktop_model::Badge;

use super::fake::FakeLink;
use super::{Backend, BackendView, Config, TICK_EVERY};
use crate::driver::clock::{Clock, ManualClock};
use crate::driver::fake::FakeApi;
use crate::driver::shared::handle::OnChange;

pub(super) const PATIENCE: Duration = Duration::from_secs(10);
pub(super) const T0: u64 = 1_700_000_000_000;

pub(super) struct Rig {
    pub clock: Arc<ManualClock>,
    pub backend: Option<Backend>,
    /// How many times the backend's `on_change` was called.
    pub changes: Arc<AtomicU64>,
}

impl Rig {
    pub(super) fn start(link: Arc<FakeLink>) -> Rig {
        let clock = Arc::new(ManualClock::new(T0));
        Rig::start_on(link, clock.clone(), clock, None)
    }

    /// A rig whose backend reads `read` as its clock while the test moves
    /// `clock` (the same clock, or one that wraps it), and whose `on_change`
    /// also calls `also`.
    pub(super) fn start_on(
        link: Arc<FakeLink>,
        clock: Arc<ManualClock>,
        read: Arc<dyn Clock>,
        also: Option<OnChange>,
    ) -> Rig {
        let changes = Arc::new(AtomicU64::new(0));
        let seen = Arc::clone(&changes);
        let cfg = Config {
            on_change: Some(Arc::new(move || {
                seen.fetch_add(1, Ordering::SeqCst);
                if let Some(also) = &also {
                    also();
                }
            })),
            // The longest the drivers accept: a test that moves the clock by
            // a minute must not make them refresh by accident.
            approvals_refresh: Duration::from_secs(3_600),
            skills_refresh: Duration::from_secs(3_600),
        };
        let backend = Backend::start(link.clone(), read, cfg).expect("start");
        // The ticker is asleep before the test moves the time.
        assert!(clock.wait_sleeps_begun(1, PATIENCE));
        Rig {
            clock,
            backend: Some(backend),
            changes,
        }
    }

    pub(super) fn backend(&self) -> &Backend {
        self.backend.as_ref().expect("running")
    }

    pub(super) fn view(&self) -> BackendView {
        self.backend().snapshot()
    }

    /// Wait until the view satisfies `pred`; returns it.
    pub(super) fn until(&self, why: &str, pred: impl Fn(&BackendView) -> bool) -> BackendView {
        let end = Instant::now() + PATIENCE;
        loop {
            let v = self.view();
            if pred(&v) {
                return v;
            }
            assert!(Instant::now() < end, "timed out waiting for: {why}; {v:?}");
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    /// Wait until `ok()`.
    pub(super) fn wait(&self, why: &str, ok: impl Fn(&Backend) -> bool) {
        let end = Instant::now() + PATIENCE;
        while !ok(self.backend()) {
            assert!(Instant::now() < end, "timed out waiting for: {why}");
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    /// Wait until `ok()`, moving the clock a tick at a time meanwhile: for a
    /// wait on something the backend does when time has passed (the gap before
    /// an attach, a backoff). A tick can be missed when the test moves the
    /// clock again before the ticker has slept again; ticking until the thing
    /// happens does not depend on any one of them.
    pub(super) fn wait_ticking(&self, why: &str, ok: impl Fn(&Backend) -> bool) {
        let end = Instant::now() + PATIENCE;
        while !ok(self.backend()) {
            assert!(Instant::now() < end, "timed out waiting for: {why}");
            self.tick();
            std::thread::yield_now();
        }
    }

    /// [`Rig::until`] that moves the clock a tick at a time (see
    /// [`Rig::wait_ticking`]).
    pub(super) fn until_ticking(
        &self,
        why: &str,
        pred: impl Fn(&BackendView) -> bool,
    ) -> BackendView {
        self.wait_ticking(why, |b| pred(&b.snapshot()));
        self.view()
    }

    /// One backend tick of the clock.
    pub(super) fn tick(&self) {
        self.advance(TICK_EVERY);
    }

    /// Move the clock by `d`, one tick at a time, and let each tick be seen.
    pub(super) fn advance(&self, d: Duration) {
        let mut left = d;
        while !left.is_zero() {
            let step = left.min(TICK_EVERY);
            let before = self.clock.sleeps_begun();
            self.clock.advance(step);
            // The ticker slept again: its tick was sent.
            assert!(
                self.clock.wait_sleeps_begun(before + 1, PATIENCE),
                "the ticker sleeps again"
            );
            left -= step;
        }
    }
}

/// An approvals answer with no request.
pub(super) fn no_approvals() -> ApprovalList {
    serde_json::from_value(json!({ "approvals": [] })).expect("a list")
}

/// An answer with `n` drafts waiting and one approved skill.
pub(super) fn skills(n: usize) -> SkillsState {
    let drafts: Vec<_> = (0..n)
        .map(|i| {
            json!({"id": format!("d{i}"), "name": "a draft", "description": "How to",
                "created_at": "2026-11-17", "hash": "sha256:cd", "tainted": false,
                "warnings": [], "nearest": null})
        })
        .collect();
    let skill = json!({"name": "report", "state": "approved", "status": "approved",
        "hash": "sha256:aa", "approved_hash": null, "approved_at": null, "source": null,
        "source_text": "", "history": []});
    serde_json::from_value(
        json!({"skills": [skill], "drafts": drafts, "archived": [], "notes": []}),
    )
    .expect("a skills answer")
}

/// An answer with one trusted tool and `n` that are not.
pub(super) fn tools(n: usize) -> ToolsState {
    let tool = |name: String, trust: &str, state: &str| {
        json!({"trust": trust, "name": name, "state": state, "tier": "user",
            "source": null, "version": null, "approved_at": null, "secrets": [],
            "sandbox": "none", "manifest_hash": null, "lock": "-"})
    };
    let mut servers = vec![tool("cora".into(), "trusted", "approved")];
    servers.extend((0..n).map(|i| tool(format!("t{i}"), "not_trusted", "unapproved")));
    serde_json::from_value(json!({"servers": servers, "notes": [], "probed": false}))
        .expect("a tools answer")
}

/// An api that answers the first loads: no approvals, `drafts` skills drafts
/// waiting, `untrusted` tools not trusted.
pub(super) fn known_api(drafts: usize, untrusted: usize) -> Arc<FakeApi> {
    let api = FakeApi::new();
    api.reply_list(Ok(no_approvals()));
    api.reply_skills(Ok(skills(drafts)));
    api.reply_tools(Ok(tools(untrusted)));
    api
}

/// Whether the view knows everything `known_api(drafts, untrusted)` told.
pub(super) fn is_known(v: &BackendView, drafts: usize, untrusted: usize) -> bool {
    v.approvals.badge == Badge::Count(0)
        && v.skills.skills_waiting == Badge::Count(drafts)
        && v.skills.tools_needing_attention == Badge::Count(untrusted)
}

/// Whether the view knows nothing the drivers could have told.
pub(super) fn is_unknown(v: &BackendView) -> bool {
    v.approvals.badge == Badge::Unknown
        && v.skills.skills_waiting == Badge::Unknown
        && v.skills.tools_needing_attention == Badge::Unknown
}
