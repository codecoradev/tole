//! The pieces the approvals, skills and task drivers have in common, each in
//! one place: a driver keeps only what differs (its `Input`, its tick, its
//! bounds).
//!
//! - [`ids`]: which session ids the drivers and the backend accept.
//! - [`now`]: the two readings of time the worker takes at every input.
//! - [`thread`]: the thread bodies and the stop sequence every driver runs,
//!   generic over the driver's `Input` through [`thread::DriverInput`].
//! - [`lane`]: the schedule of one route's loads (pace, backoff, refresh, age),
//!   for the drivers that load a list on a timer.
//! - [`handle`]: what the UI's side shares: [`handle::Busy`],
//!   [`handle::OnChange`], the non-blocking queue behind every command, the
//!   change callback and the refresh bounds.

pub(crate) mod handle;
pub(crate) mod ids;
pub(crate) mod lane;
pub(crate) mod now;
pub(crate) mod thread;
