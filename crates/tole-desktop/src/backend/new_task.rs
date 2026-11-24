//! Starting a task: a new session on the serve, opened as a task.
//!
//! The one call on the serve the app makes itself (every other call is a
//! driver's): `POST /sessions`. It may take as long as a call does, so it runs
//! on a short-lived thread, and its result is the next view: the session is in
//! [`Backend::open_tasks`] when it worked, and [`Backend::new_task_failed`] says
//! when it did not. The window shows the task that appeared (`Tabs::watch`).

use std::fmt;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::thread;

use tole_client::NewSession;

use super::shared::{Msg, Shared};
use super::Backend;
use crate::driver::task::MAX_OPEN_TASKS;

/// Why [`Backend::new_task`] did not start one. Nothing was asked of the serve.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NewTaskRefused {
    /// [`MAX_OPEN_TASKS`] tasks are open: the window says so and offers to close one.
    TooMany { max: usize },
    /// The serve is not ready (starting, restarting, failed): there is nothing to ask.
    NotReady,
    /// A task is being started already (one call at a time).
    Busy,
    /// The backend has stopped.
    Stopped,
    /// The system could not start the thread that makes the call.
    NoThread,
}

impl fmt::Display for NewTaskRefused {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            NewTaskRefused::TooMany { max } => write!(f, "at most {max} tasks can be open"),
            NewTaskRefused::NotReady => f.write_str("Tole is not ready"),
            NewTaskRefused::Busy => f.write_str("a task is being started"),
            NewTaskRefused::Stopped => f.write_str("the backend has stopped"),
            NewTaskRefused::NoThread => f.write_str("could not start a thread"),
        }
    }
}

impl std::error::Error for NewTaskRefused {}

/// The one place a task is being started. Taking it is the check that no other
/// call is out; dropping it, on any path, frees the place, and a call that
/// ended without saying how (a panic) counts as failed.
struct Slot {
    shared: Arc<Shared>,
    /// The call is out: its end decides the failure flag.
    armed: bool,
    outcome: Option<bool>,
}

impl Slot {
    fn take(shared: &Arc<Shared>) -> Result<Slot, NewTaskRefused> {
        let free =
            shared
                .new_task_busy
                .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst);
        match free {
            Ok(_) => Ok(Slot {
                shared: Arc::clone(shared),
                armed: false,
                outcome: None,
            }),
            Err(_) => Err(NewTaskRefused::Busy),
        }
    }

    fn done(&mut self, worked: bool) {
        self.outcome = Some(worked);
    }
}

impl Drop for Slot {
    fn drop(&mut self) {
        // Only the call that holds the place writes the flag, and it does so
        // before the place is free: an older attempt cannot overwrite a newer.
        if self.armed {
            let failed = !self.outcome.unwrap_or(false);
            self.shared.new_task_failed.store(failed, Ordering::SeqCst);
        }
        self.shared.new_task_busy.store(false, Ordering::SeqCst);
    }
}

impl Backend {
    /// The tasks the app has open, in the order it opened them.
    pub fn open_tasks(&self) -> Vec<String> {
        self.shared.tasks.list()
    }

    /// Whether the last [`new_task`](Self::new_task) failed on the serve (it
    /// answered an error or not at all). Cleared by the next attempt.
    pub fn new_task_failed(&self) -> bool {
        self.shared.new_task_failed.load(Ordering::SeqCst)
    }

    /// Make a session in `cwd` (the serve's workspace when `None`) and open it
    /// as a task, off this thread. `Ok` only means the call was started: the
    /// task shows in [`open_tasks`](Self::open_tasks) when the serve made it,
    /// and [`new_task_failed`](Self::new_task_failed) says when it did not.
    /// Refused at once, asking nothing, when another call is still out
    /// ([`NewTaskRefused::Busy`]: one at a time, so two clicks make one
    /// session), when [`MAX_OPEN_TASKS`] are open (the
    /// session would be made and not followed) or when the serve is not ready.
    pub fn new_task(&self, cwd: Option<&str>) -> Result<(), NewTaskRefused> {
        if !self.is_running() {
            return Err(NewTaskRefused::Stopped);
        }
        // First the place, then the checks it protects: the cap is read while
        // no other call can open a session.
        let mut slot = Slot::take(&self.shared)?;
        if self.shared.tasks.list().len() >= MAX_OPEN_TASKS {
            return Err(NewTaskRefused::TooMany {
                max: MAX_OPEN_TASKS,
            });
        }
        let look = self.link.look();
        let attached = self.shared.current(&look).ok_or(NewTaskRefused::NotReady)?;
        let request = NewSession {
            cwd: cwd.map(str::to_owned),
            ..NewSession::default()
        };
        let (fan, inbox) = (Arc::clone(&self.fan), self.inbox.clone());
        self.shared.new_task_failed.store(false, Ordering::SeqCst);
        slot.armed = true;
        thread::Builder::new()
            .name("backend-new-task".to_owned())
            .spawn(move || {
                let opened = attached.api.create_session(&request);
                let worked = opened
                    .ok()
                    .is_some_and(|session| slot.shared.tasks.open(&session.session_id).is_ok());
                slot.done(worked);
                drop(slot);
                fan.mark();
                let _ = inbox.try_send(Msg::Wake);
            })
            .map(|_| ())
            .map_err(|_| NewTaskRefused::NoThread)
    }
}
