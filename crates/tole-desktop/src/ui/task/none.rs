//! The task view where there is no driver (not macOS or Linux): no panes.

use crate::ui::adapter::{AnyElement, Context, Window};

use crate::ui::shell::ShellView;

impl ShellView {
    /// There is no task pane.
    pub(in crate::ui) fn task_pane(
        &mut self,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        None
    }
}
