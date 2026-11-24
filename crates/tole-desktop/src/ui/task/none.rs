//! The task view where there is no driver (not macOS or Linux): no panes.

use crate::ui::adapter::{AnyElement, Context};

use crate::ui::shell::ShellView;

/// No open tasks: a window with no driver has none.
#[derive(Default)]
pub(in crate::ui) struct TaskPanes;

impl ShellView {
    /// There is no task pane.
    pub(in crate::ui) fn task_pane(&mut self, _cx: &mut Context<Self>) -> Option<AnyElement> {
        None
    }
}
