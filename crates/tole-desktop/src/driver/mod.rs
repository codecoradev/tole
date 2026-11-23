//! The drivers: the threads between the typed client and the model.
//!
//! The adapter (`crate::adapter`) is pure: client value in, model value out.
//! The model (`tole-desktop-model`) is pure: state and reducers. A driver is
//! what owns a connection and the time: it calls the client, feeds the replies
//! through the adapter into the model, retries what failed, and hands the UI a
//! plain-data view. No toolkit type appears here.
//!
//! One module per page, on a shared foundation:
//!
//! - [`api`]: the calls a driver may make, as the [`Api`] trait, so a test
//!   scripts the serve without a socket; [`ClientApi`] is the real client with
//!   the call bounds a driver relies on.
//! - [`clock`]: the [`Clock`] a driver reads and sleeps on, and the
//!   [`Shutdown`] signal that interrupts every sleep.
//! - [`runtime`]: starting threads together and stopping them with a timeout.
//! - [`guard`]: containing a panic in a call, and the probing of a refused stream.
//! - `shared`: what the drivers have in common below that: the `Now` pair of
//!   clock readings, the tick, guarded-thread and stop sequence, and the UI
//!   side's `Busy`, `OnChange` and refresh clamp.
//! - [`approvals`]: the approvals queue (the first driver).
//! - [`skills`]: the Skills and Connected tools pages (read-only).
//! - [`task`]: one task, that is one session: its history, live stream, prompt,
//!   Stop, steer and the approvals of that session.
//!
//! Rules every driver keeps (the thread model of each is in its module docs):
//!
//! - No blocking call on the thread that owns the state, and none while a
//!   lock is held: a call may take up to a minute.
//! - Deadlines are on the monotonic clock; the wall clock only feeds the
//!   model's `now_ms`, so a clock correction cannot stall a retry.
//! - A failed call is retried with a growing wait, never in a loop, and never
//!   given up. Until a call succeeds the model says "unknown", never a number.
//! - Channels are bounded; a thread that waits does so on a condvar or a
//!   channel, never by polling.
//! - Server data never panics a thread. A panic inside a call is that call's
//!   failure; a panic anywhere else ends the driver, which leaves the model
//!   cleared (unknown), not stale.

pub mod api;
pub mod approvals;
pub mod clock;
pub mod guard;
pub mod runtime;
pub(crate) mod shared;
pub mod skills;
pub mod task;

#[cfg(test)]
mod api_tests;
#[cfg(test)]
pub(crate) mod fake;
#[cfg(test)]
mod foundation_tests;
#[cfg(test)]
mod runtime_tests;

pub use api::{Api, ClientApi, EventSource};
pub use clock::{Clock, ManualClock, Shutdown, Slept, SystemClock};
