//! The page state shared between the worker and the UI's [`Handle`](super::Handle).

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, MutexGuard};

use tole_desktop_model::{AutomationId, AutomationsState, Load};

use super::view::AutomationsView;
pub use crate::driver::shared::handle::Feed;

/// What the driver knows about the history of the selected automation, beside
/// `AutomationsState::history` (the model has no state for it).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct History {
    /// `Idle` while nothing is selected, `Loading` until the first answer for
    /// the selection, `Ready`, or `Failed` (the entries, if any, are then the
    /// last ones read and are not current).
    pub load: Load,
    /// At least this many older runs exist that were not kept (see
    /// `adapter::automations::Runs::more_at_least`).
    pub more_at_least: usize,
    /// Whether the link of every run that stopped to ask could be looked up.
    /// When false a run without a link is "not known", not "nothing waits".
    pub links_known: bool,
    pub feed: Feed,
}

impl Default for History {
    fn default() -> Self {
        History {
            load: Load::Idle,
            more_at_least: 0,
            links_known: true,
            feed: Feed::default(),
        }
    }
}

/// The page and what the driver adds to it change together under one lock, so
/// a view never shows a count of failures that belongs to another state.
#[derive(Debug, Default)]
pub(super) struct Shared {
    pub page: AutomationsState,
    /// Jobs of the last answer that are not among the rows (past the cap, or a
    /// repeated id): the rows are not all of them.
    pub left_out: usize,
    pub jobs: Feed,
    pub history: History,
    /// The automation whose switch waits for the serve's answer.
    pub toggling: Option<AutomationId>,
}

/// Locked only for a bounded, I/O-free piece of work. A panic while it was held
/// may have left the state half changed, so the next lock throws it away: the
/// fresh page is not loaded (no rows), the careful state, and the engine is
/// told to load again.
#[derive(Default)]
pub(super) struct Store {
    shared: Mutex<Shared>,
    reset: AtomicBool,
}

impl Store {
    pub(super) fn lock(&self) -> MutexGuard<'_, Shared> {
        match self.shared.lock() {
            Ok(guard) => guard,
            Err(poisoned) => {
                let mut guard = poisoned.into_inner();
                *guard = Shared::default();
                self.shared.clear_poison();
                self.reset.store(true, Ordering::SeqCst);
                guard
            }
        }
    }

    /// Forget everything (the driver stopped, or died): not loaded.
    pub(super) fn clear(&self) {
        *self.lock() = Shared::default();
    }

    pub(super) fn view(&self) -> AutomationsView {
        AutomationsView::of(&self.lock())
    }

    /// Whether the state was thrown away since the last call.
    pub(super) fn take_reset(&self) -> bool {
        // A lock finds a poisoned state and sets the flag: look first.
        drop(self.lock());
        self.reset.swap(false, Ordering::SeqCst)
    }
}
