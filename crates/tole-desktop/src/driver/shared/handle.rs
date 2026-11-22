//! What the UI's side of every driver shares: the refusal a command can get,
//! the change callback, and the clamping of the refresh.

use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::mpsc::{SyncSender, TrySendError};
use std::sync::Arc;
use std::time::Duration;

/// The driver is stopped, or its inbox is full: the call did nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Busy;

/// What the UI calls when the view changed; runs on the worker thread, with no
/// lock held, so it should only ask the UI to repaint (and read the handle's
/// `snapshot` there).
pub type OnChange = Arc<dyn Fn() + Send + Sync>;

/// Queue `input` for the worker without blocking: [`Busy`] when its inbox is
/// full or closed.
pub(crate) fn queue<M>(inbox: &SyncSender<M>, input: M) -> Result<(), Busy> {
    match inbox.try_send(input) {
        Ok(()) => Ok(()),
        Err(TrySendError::Full(_) | TrySendError::Disconnected(_)) => Err(Busy),
    }
}

/// Tell the UI the view changed. The UI's callback must not take the worker
/// down with it: a panic in it is contained.
pub(crate) fn notify(on_change: &Option<OnChange>) {
    if let Some(notify) = on_change {
        let _ = catch_unwind(AssertUnwindSafe(|| notify()));
    }
}

/// The refresh a driver uses: `refresh` held between `min` and `ceiling` (the
/// driver's maximum, but in a test). A ceiling below `min` is `min`.
pub(crate) fn clamp_refresh(refresh: Duration, min: Duration, ceiling: Duration) -> Duration {
    refresh.clamp(min, ceiling.max(min))
}
