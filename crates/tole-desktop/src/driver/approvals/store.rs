//! The model state shared between the worker and the UI's [`Handle`](super::Handle).

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Mutex, MutexGuard};

use tole_desktop_model::approvals::ApprovalsState;

use super::view::ApprovalsView;

/// Locked only for a bounded, I/O-free piece of work. A panic while it was held
/// may have left the state half changed, so the next lock throws it away: the
/// fresh state is `Unknown` and every message box `Checking`, the careful
/// state, and the engine is told to fetch again.
#[derive(Default)]
pub(in crate::driver::approvals) struct Store {
    state: Mutex<ApprovalsState>,
    reset: AtomicBool,
    /// Fetches that failed in a row (see `ApprovalsView::fetch_failures`).
    failures: AtomicU32,
    /// Followed sessions covered by the refresh only (`ApprovalsView::parked_sessions`).
    parked: Mutex<Vec<String>>,
}

impl Store {
    pub(in crate::driver::approvals) fn lock(&self) -> MutexGuard<'_, ApprovalsState> {
        match self.state.lock() {
            Ok(guard) => guard,
            Err(poisoned) => {
                let mut guard = poisoned.into_inner();
                *guard = ApprovalsState::new();
                self.state.clear_poison();
                self.reset.store(true, Ordering::SeqCst);
                guard
            }
        }
    }

    /// Forget everything (the driver stopped, or died): unknown.
    pub(in crate::driver::approvals) fn clear(&self) {
        *self.lock() = ApprovalsState::new();
        self.failures.store(0, Ordering::SeqCst);
        self.set_parked(Vec::new());
    }

    /// The view as of the wall-clock time `now`.
    pub(in crate::driver::approvals) fn view(&self, now: u64) -> ApprovalsView {
        let failures = self.failures.load(Ordering::SeqCst);
        let parked = self
            .parked
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone();
        ApprovalsView::of(&self.lock(), now, failures, parked)
    }

    pub(super) fn set_failures(&self, n: u32) -> bool {
        self.failures.swap(n, Ordering::SeqCst) != n
    }

    pub(super) fn set_parked(&self, parked: Vec<String>) -> bool {
        let mut now = self.parked.lock().unwrap_or_else(|p| p.into_inner());
        std::mem::replace(&mut *now, parked) != *now
    }

    /// Whether the state was thrown away since the last call.
    pub(super) fn take_reset(&self) -> bool {
        // A lock finds a poisoned state and sets the flag: look first.
        drop(self.lock());
        self.reset.swap(false, Ordering::SeqCst)
    }
}
