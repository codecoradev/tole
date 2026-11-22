//! The pieces the approvals, skills and task drivers have in common, each in
//! one place: a driver keeps only what differs (its `Input`, its tick, its
//! bounds).
//!
//! - [`now`]: the two readings of time the worker takes at every input.
//! - [`thread`]: the thread bodies and the stop sequence every driver runs,
//!   generic over the driver's `Input` through [`thread::DriverInput`].
//! - [`handle`]: what the UI's side shares: [`handle::Busy`],
//!   [`handle::OnChange`], the non-blocking queue behind every command, the
//!   change callback and the refresh bounds.

pub(crate) mod handle;
pub(crate) mod now;
pub(crate) mod thread;
