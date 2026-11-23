//! The page state shared between the worker and the UI's [`Handle`](super::Handle).

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, MutexGuard};

use tole_desktop_model::SkillsPageState;

use super::view::SkillsPageView;
pub use crate::driver::shared::handle::Feed;

/// The page and the feeds change together under one lock, so a view never
/// shows a count of failures that belongs to another state of the page.
#[derive(Debug, Default)]
pub(super) struct Shared {
    pub page: SkillsPageState,
    pub skills: Feed,
    pub tools: Feed,
}

/// Locked only for a bounded, I/O-free piece of work. A panic while it was held
/// may have left the state half changed, so the next lock throws it away: the
/// fresh page is not loaded (`Unknown` badges, no rows), the careful state, and
/// the engine is told to load again.
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

    pub(super) fn view(&self) -> SkillsPageView {
        SkillsPageView::of(&self.lock())
    }

    /// Whether the state was thrown away since the last call.
    pub(super) fn take_reset(&self) -> bool {
        // A lock finds a poisoned state and sets the flag: look first.
        drop(self.lock());
        self.reset.swap(false, Ordering::SeqCst)
    }
}
