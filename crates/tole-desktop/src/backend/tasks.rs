//! The tasks the app has open: the set the backend keeps, and why it refuses.
//!
//! The set is the app's wish, not what runs: a task opened while the serve is
//! down is recorded here and started when the serve is ready, and the set
//! survives a restart of the serve (the drivers are made again for the same
//! sessions).
//!
//! # The stream budget
//!
//! The serve allows [`SERVE_STREAM_SLOTS`] event streams (8) for all its
//! clients together. The approvals driver follows exactly the open tasks'
//! sessions, so it holds one stream for each (and at most
//! [`approvals::MAX_STREAMS`], 4), and each task driver holds one: with the
//! cap of [`MAX_OPEN_TASKS`] (3) the backend holds at most 3 + 3 = 6 live
//! streams, and 7 is the bound the two constants allow (4 + 3). That leaves
//! at least one slot for the CLI or another window. The skills driver uses no
//! stream. A stream a stopped driver's reader still holds (a read cannot be
//! cancelled) counts against the serve's eight until its next frame, so a task
//! closed and opened again at once may find its stream refused (429) for a
//! moment; its driver probes and recovers.

use std::fmt;
use std::sync::{Mutex, MutexGuard};

use crate::driver::approvals;
use crate::driver::shared::ids::is_session_id;
use crate::driver::task::{MAX_OPEN_TASKS, SERVE_STREAM_SLOTS};

// The approvals readers and the task drivers never take all of the serve's slots.
const _: () = assert!(approvals::MAX_STREAMS + MAX_OPEN_TASKS < SERVE_STREAM_SLOTS);

/// Why [`Backend::open_task`](super::Backend::open_task) did nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OpenRefused {
    /// Not an id the serve can have given (`[A-Za-z0-9_-]`, 1 to 64 bytes).
    InvalidId,
    /// [`MAX_OPEN_TASKS`] tasks are open already: the UI says so and offers to
    /// close one.
    TooMany { max: usize },
    /// The backend has stopped.
    Stopped,
}

impl fmt::Display for OpenRefused {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            OpenRefused::InvalidId => f.write_str("not a task id"),
            OpenRefused::TooMany { max } => write!(f, "at most {max} tasks can be open"),
            OpenRefused::Stopped => f.write_str("the backend has stopped"),
        }
    }
}

impl std::error::Error for OpenRefused {}

/// Why a command to a task did nothing. The command was not queued.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskRefused {
    /// The task is not open ([`Backend::open_task`](super::Backend::open_task)
    /// first).
    NotOpen,
    /// The task is open but has no driver now: the serve is not ready, was
    /// replaced, or the driver is being made again. Try again when the view
    /// says the serve is ready.
    Unavailable,
    /// The driver's inbox is full, or it stopped just now: try again.
    Busy,
}

impl fmt::Display for TaskRefused {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            TaskRefused::NotOpen => "the task is not open",
            TaskRefused::Unavailable => "the task is not connected",
            TaskRefused::Busy => "the task is busy",
        })
    }
}

impl std::error::Error for TaskRefused {}

/// The open tasks, in the order they were opened. Locked for a copy or an edit
/// of a handful of short strings, never across a call.
#[derive(Default)]
pub(super) struct TaskSet(Mutex<Vec<String>>);

impl TaskSet {
    fn lock(&self) -> MutexGuard<'_, Vec<String>> {
        self.0.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Record `id` as open. `Ok(true)` when it was not open before; opening
    /// an open task is `Ok(false)` and takes no slot.
    pub(super) fn open(&self, id: &str) -> Result<bool, OpenRefused> {
        if !is_session_id(id) {
            return Err(OpenRefused::InvalidId);
        }
        let mut open = self.lock();
        if open.iter().any(|t| t == id) {
            return Ok(false);
        }
        if open.len() >= MAX_OPEN_TASKS {
            return Err(OpenRefused::TooMany {
                max: MAX_OPEN_TASKS,
            });
        }
        open.push(id.to_owned());
        Ok(true)
    }

    /// Whether `id` was open.
    pub(super) fn close(&self, id: &str) -> bool {
        let mut open = self.lock();
        let before = open.len();
        open.retain(|t| t != id);
        open.len() != before
    }

    pub(super) fn contains(&self, id: &str) -> bool {
        self.lock().iter().any(|t| t == id)
    }

    pub(super) fn list(&self) -> Vec<String> {
        self.lock().clone()
    }
}
