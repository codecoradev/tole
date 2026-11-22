//! The state shared between the worker and the UI's [`Handle`](super::Handle):
//! the model's `TaskState`, the facts the driver adds to it, and the window of
//! rows the UI is looking at. One mutex, held for the apply (or the copy for a
//! view) and nothing else: never across a call or a send.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, MutexGuard};

use tole_desktop_model::task::TaskState;

use super::view::TaskView;

/// What the driver knows besides the model.
#[derive(Debug, Clone, Default)]
pub(super) struct Facts {
    /// The history is still being read (pages, or the retry of one).
    pub loading: bool,
    /// A stream has been up at least once.
    pub connected: bool,
    /// A stream is up now (a REST read says nothing of that).
    pub stream_up: bool,
    /// The last message was given up on with no sign that it was sent: its
    /// words are back in the draft.
    pub unsent: bool,
    /// The serve keeps refusing the stream: it is probed slowly.
    pub parked: bool,
    /// The prompt call is out (the turn it serves has not been reported over).
    pub prompting: bool,
    /// A steer note is out.
    pub steering: bool,
    /// Approval ids queued for an answer or being answered.
    pub deciding: Vec<String>,
    /// Cards the driver cannot answer (see `engine::ids`).
    pub undecidable: Vec<String>,
    /// Commands the engine refused, ever (see `TaskView::ignored`).
    pub ignored: u64,
}

/// The rows the UI shows: scroll offset and height of its list, in pixels.
#[derive(Debug, Clone, Copy)]
pub(super) struct Viewport {
    pub scroll_top: f32,
    pub height: f32,
    pub overscan: f32,
}

impl Default for Viewport {
    fn default() -> Self {
        Viewport {
            scroll_top: 0.0,
            height: 800.0,
            overscan: 400.0,
        }
    }
}

pub(super) struct Shared {
    pub task: TaskState,
    pub facts: Facts,
    pub viewport: Viewport,
}

impl Shared {
    fn new(look_only: bool) -> Self {
        Shared {
            task: TaskState::new(look_only),
            facts: Facts::default(),
            viewport: Viewport::default(),
        }
    }
}

/// A panic while the lock was held may have left the model half changed: the
/// next lock notes it (`broken`) and the engine ends the driver, which leaves
/// the view stopped, instead of showing a state nobody can vouch for.
pub(super) struct Store {
    shared: Mutex<Shared>,
    look_only: bool,
    broken: AtomicBool,
}

impl Store {
    pub(super) fn new(look_only: bool) -> Self {
        Store {
            shared: Mutex::new(Shared::new(look_only)),
            look_only,
            broken: AtomicBool::new(false),
        }
    }

    pub(super) fn lock(&self) -> MutexGuard<'_, Shared> {
        self.shared.lock().unwrap_or_else(|poisoned| {
            self.broken.store(true, Ordering::SeqCst);
            self.shared.clear_poison();
            poisoned.into_inner()
        })
    }

    /// Whether the lock was found poisoned since the last call.
    pub(super) fn take_broken(&self) -> bool {
        drop(self.lock());
        self.broken.swap(false, Ordering::SeqCst)
    }

    /// Forget everything (the driver stopped, or died).
    pub(super) fn clear(&self) {
        *self.lock() = Shared::new(self.look_only);
    }

    pub(super) fn view(&self, alive: bool) -> TaskView {
        TaskView::of(&self.lock(), alive)
    }
}
