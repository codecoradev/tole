//! The window's tasks through the app: the open set, a task's view, what the
//! owner does in it, and "New task". The window reads these through
//! [`Source`](crate::ui::Source); every one returns at once.

use super::App;
use crate::backend::{NewTaskRefused, TaskRefused};
use crate::driver::task::TaskView;
use crate::ui::TaskCall;
use tole_desktop_model::strings::StartProblem;

impl App {
    /// The tasks open, in the order they were opened.
    pub(super) fn tasks_open(&self) -> Vec<String> {
        self.backend.open_tasks()
    }

    /// The window lets go of `session` (its driver stops).
    pub(super) fn tasks_close(&self, session: &str) {
        self.backend.close_task(session);
    }

    pub(super) fn tasks_view(&self, session: &str) -> Option<TaskView> {
        self.backend.task_view(session)
    }

    /// What the owner or the list did in `session`; refused with the reason
    /// when the task is not open or has no driver now.
    pub(super) fn tasks_call(&self, session: &str, call: TaskCall) -> Result<(), TaskRefused> {
        let b = &self.backend;
        if !b.open_tasks().iter().any(|t| t == session) {
            return Err(TaskRefused::NotOpen);
        }
        let driver = |result: Option<Result<(), _>>| match result {
            None => Err(TaskRefused::Unavailable),
            Some(Err(_)) => Err(TaskRefused::Busy),
            Some(Ok(())) => Ok(()),
        };
        match call {
            TaskCall::Send(text) => b.send(session, &text),
            TaskCall::Steer(text) => b.steer(session, &text),
            TaskCall::Stop => b.stop(session),
            TaskCall::Toggle(row) => driver(b.with_task(session, |h| h.toggle_group(row))),
            TaskCall::Heights(rows) => driver(b.with_task(session, |h| h.set_heights(rows))),
            TaskCall::Viewport {
                top,
                height,
                overscan,
            } => {
                // Not a command: it changes what the next views hold, and cannot be refused.
                let set = b.with_task(session, |h| h.set_viewport(top, height, overscan));
                set.ok_or(TaskRefused::Unavailable)
            }
        }
    }

    /// "New task": a session in `folder` (the serve's workspace when `None`),
    /// opened as a task when the serve made it.
    pub(super) fn tasks_new(&self, folder: Option<&str>) {
        let refused = self.backend.new_task(folder).err();
        *self
            .new_task_refused
            .lock()
            .unwrap_or_else(|p| p.into_inner()) = refused;
        self.signal.raise();
    }

    /// Why the last "New task" did not start a task: it was refused at once,
    /// or the serve could not make the session (see [`App::tasks_new`]).
    pub(super) fn tasks_new_problem(&self) -> Option<StartProblem> {
        let refused = *self
            .new_task_refused
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        match refused {
            Some(NewTaskRefused::TooMany { max }) => Some(StartProblem::TooMany { max }),
            Some(NewTaskRefused::NotReady | NewTaskRefused::Stopped) => {
                Some(StartProblem::NotReady)
            }
            Some(NewTaskRefused::Busy) => Some(StartProblem::Busy),
            Some(NewTaskRefused::NoThread) => Some(StartProblem::Other),
            None => self
                .backend
                .new_task_failed()
                .then_some(StartProblem::ServeFailed),
        }
    }
}
