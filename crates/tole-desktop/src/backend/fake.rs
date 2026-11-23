//! A scripted [`ServeLink`] for the backend's tests: no supervisor, no serve.
//!
//! The test flips the state by hand ([`FakeLink::set`], which counts an epoch
//! the way the supervisor's observer does), hands out the [`FakeApi`]s the
//! drivers will call, and can make the link refuse an api or panic. It counts
//! what the backend asked of it, and the test waits on those counts (bounded in
//! real time only, so a broken backend fails a test instead of hanging it).

use std::collections::VecDeque;
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::Duration;

use tole_desktop_model::ServeState;

use super::link::ServeLink;
use crate::driver::api::Api;
use crate::driver::fake::FakeApi;

const PATIENCE: Duration = Duration::from_secs(10);

pub(super) fn running(version: &str) -> ServeState {
    ServeState::Running {
        version: version.to_owned(),
    }
}

pub(super) fn restarting() -> ServeState {
    ServeState::Restarting {
        attempt: 1,
        retry_in: Duration::from_secs(1),
    }
}

struct Inner {
    state: ServeState,
    epoch: u64,
    /// The apis the next attaches get, in order; a fresh one when none is left.
    queued: VecDeque<Arc<FakeApi>>,
    /// Every api handed out, in order.
    given: Vec<Arc<FakeApi>>,
    api_calls: usize,
    /// How many times the epoch was read: once per look of the backend.
    looks: usize,
    /// The next this many `api()` calls answer `None`.
    refuse: usize,
    panic_state: bool,
    panic_api: bool,
}

pub(super) struct FakeLink {
    inner: Mutex<Inner>,
    changed: Condvar,
}

impl FakeLink {
    pub(super) fn new(state: ServeState) -> Arc<FakeLink> {
        Arc::new(FakeLink {
            inner: Mutex::new(Inner {
                state,
                epoch: 1,
                queued: VecDeque::new(),
                given: Vec::new(),
                api_calls: 0,
                looks: 0,
                refuse: 0,
                panic_state: false,
                panic_api: false,
            }),
            changed: Condvar::new(),
        })
    }

    /// A link whose serve is running.
    pub(super) fn ready() -> Arc<FakeLink> {
        FakeLink::new(running("1"))
    }

    fn inner(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// The serve is now in `state`. A different state is a new epoch, as for
    /// the supervisor's observer (which counts every change).
    pub(super) fn set(&self, state: ServeState) {
        let mut inner = self.inner();
        if inner.state != state {
            inner.state = state;
            inner.epoch += 1;
        }
    }

    /// The serve is now in `state`, and the epoch does not move: a link that
    /// breaks its contract (the state of the view must still not outlive it).
    pub(super) fn set_without_epoch(&self, state: ServeState) {
        self.inner().state = state;
    }

    /// The api the next attach that has none queued before it gets.
    pub(super) fn queue_api(&self, api: Arc<FakeApi>) {
        self.inner().queued.push_back(api);
    }

    /// The next `n` calls of `api()` answer `None`.
    pub(super) fn refuse_api(&self, n: usize) {
        self.inner().refuse = n;
    }

    pub(super) fn panic_on_state(&self, on: bool) {
        self.inner().panic_state = on;
    }

    pub(super) fn panic_on_api(&self, on: bool) {
        self.inner().panic_api = on;
    }

    /// How many times `api()` was called.
    pub(super) fn api_calls(&self) -> usize {
        self.inner().api_calls
    }

    /// How many times the backend (or a snapshot) looked at the link.
    pub(super) fn looks(&self) -> usize {
        self.inner().looks
    }

    /// The apis handed out so far.
    pub(super) fn given(&self) -> Vec<Arc<FakeApi>> {
        self.inner().given.clone()
    }

    /// Wait until `api()` was called `n` times.
    pub(super) fn wait_api_calls(&self, n: usize) {
        let guard = self.inner();
        let (guard, out) = self
            .changed
            .wait_timeout_while(guard, PATIENCE, |i| i.api_calls < n)
            .unwrap_or_else(|p| p.into_inner());
        assert!(
            !out.timed_out(),
            "timed out waiting for {n} api calls (saw {})",
            guard.api_calls
        );
    }
}

impl ServeLink for FakeLink {
    fn state(&self) -> ServeState {
        let (state, boom) = {
            let inner = self.inner();
            (inner.state.clone(), inner.panic_state)
        };
        // Never while the lock is held: a panic would poison it for the test.
        if boom {
            panic!("a scripted panic in the link's state");
        }
        state
    }

    fn epoch(&self) -> u64 {
        let mut inner = self.inner();
        inner.looks += 1;
        inner.epoch
    }

    fn api(&self) -> Option<Arc<dyn Api>> {
        let (api, boom) = {
            let mut inner = self.inner();
            inner.api_calls += 1;
            self.changed.notify_all();
            let boom = inner.panic_api;
            let api = if boom || inner.refuse > 0 {
                inner.refuse = inner.refuse.saturating_sub(1);
                None
            } else {
                let api = inner.queued.pop_front().unwrap_or_default();
                inner.given.push(Arc::clone(&api));
                Some(api)
            };
            (api, boom)
        };
        if boom {
            panic!("a scripted panic in the link's api");
        }
        api.map(|api| api as Arc<dyn Api>)
    }
}
