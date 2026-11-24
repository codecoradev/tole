//! The task view where there is no driver (not macOS or Linux): no panes.

use crate::ui::adapter::{AnyElement, Context, Window};

use crate::ui::shell::ShellView;
use tole_desktop_model::strings::StartProblem;

impl ShellView {
    /// There is no task pane.
    pub(in crate::ui) fn task_pane(
        &mut self,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        None
    }

    /// There is no app to start a task: the button does nothing.
    pub(in crate::ui) fn task_start(&mut self, _cx: &mut Context<Self>) {}

    /// Nothing was tried, so nothing failed.
    pub(in crate::ui) fn start_problem(&self) -> Option<StartProblem> {
        None
    }
}
