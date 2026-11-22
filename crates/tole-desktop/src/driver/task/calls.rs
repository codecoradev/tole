//! The calls the engine asks the I/O threads for, and what comes back.
//!
//! Each [`Lane`] is one thread with one call in flight (a lane's flag is set
//! when the engine hands it a call and cleared when its reply is applied). A
//! panic inside a call is that call's failure, never the thread's.

use tole_client::{ApprovalList, EntriesPage, Error};

use crate::driver::api::Api;
use crate::driver::guard::{guarded, panicked};

/// The threads that make calls: one call at a time on each.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Lane {
    /// History pages and the approvals queue (never both at once: the queue is
    /// fetched only after the history is in).
    Read,
}

pub(super) const LANES: [Lane; 1] = [Lane::Read];

impl Lane {
    pub(super) fn index(self) -> usize {
        self as usize
    }

    pub(super) fn name(self) -> &'static str {
        match self {
            Lane::Read => "task-read",
        }
    }
}

pub(super) enum Call {
    Entries { after: u64 },
    Approvals { token: u64 },
}

pub(super) enum Reply {
    Entries(u64, Result<EntriesPage, Error>),
    Approvals(u64, Result<ApprovalList, Error>),
}

fn caught<T>(call: impl FnOnce() -> Result<T, Error>) -> Result<T, Error> {
    guarded(call).unwrap_or_else(|| Err(panicked()))
}

/// Make one call on the serve; blocks. Never panics.
pub(super) fn run(api: &dyn Api, session: &str, call: Call) -> Reply {
    match call {
        Call::Entries { after } => {
            Reply::Entries(after, caught(|| api.entries(session, after, Some(PAGE))))
        }
        Call::Approvals { token } => {
            Reply::Approvals(token, caught(|| api.approvals(Some(session))))
        }
    }
}

/// Entries asked for per history page (serve's own most).
const PAGE: usize = 500;
