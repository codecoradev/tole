//! The two readings of time at one moment.

use std::time::Duration;

use crate::driver::clock::Clock;

/// The two readings of time at one moment. Deadlines use `mono`; `wall` is what
/// the model is given, and what shows a wall clock that jumped (a suspend or a
/// correction).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Now {
    pub wall: u64,
    pub mono: u64,
}

impl Now {
    /// Both readings of `clock`, taken together.
    pub(crate) fn read(clock: &dyn Clock) -> Self {
        Now {
            wall: clock.now_ms(),
            mono: clock.monotonic_ms(),
        }
    }
}

/// A duration in milliseconds, saturating.
pub(crate) fn millis(d: Duration) -> u64 {
    u64::try_from(d.as_millis()).unwrap_or(u64::MAX)
}
