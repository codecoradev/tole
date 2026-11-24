//! Keyboard focus order of the app shell.
//!
//! The order is part of the model, so a test can check what the window really
//! does against it: Tab moves through the header (the sidebar toggle), the
//! sidebar sections in sidebar order, the folders in list order, and then the
//! controls of the home page. A control that is not on screen is not a stop.

use crate::{FolderId, Section, ShellState, Tab};

/// A control of the shell that can take keyboard focus.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FocusTarget {
    /// The button of the header that shows or hides the sidebar.
    SidebarToggle,
    Section(Section),
    Folder(FolderId),
    /// A folder chip of the home page.
    HomeFolder(FolderId),
    /// The "New task" button of the home page.
    HomeStart,
    /// A tab of the Skills and tools page.
    SkillsTab(Tab),
    /// The refresh button of the Skills and tools page.
    SkillsRefresh,
}

impl ShellState {
    /// The controls Tab visits, first to last.
    pub fn tab_order(&self) -> Vec<FocusTarget> {
        let mut order = vec![FocusTarget::SidebarToggle];
        if self.sidebar_visible() {
            order.extend(Section::ALL.iter().map(|s| FocusTarget::Section(*s)));
            order.extend(self.folders.iter().map(|f| FocusTarget::Folder(f.id)));
        }
        if self.section == Section::NewTask {
            order.extend(self.folders.iter().map(|f| FocusTarget::HomeFolder(f.id)));
            if self.home().can_start {
                order.push(FocusTarget::HomeStart);
            }
        }
        if self.section == Section::SkillsAndTools {
            order.push(FocusTarget::SkillsTab(Tab::Skills));
            order.push(FocusTarget::SkillsTab(Tab::ConnectedTools));
            order.push(FocusTarget::SkillsRefresh);
        }
        order
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Action, ConnectionStatus};

    #[test]
    fn the_toggle_comes_first_then_sections_then_folders_then_the_home_page() {
        let state = ShellState::placeholder();
        let order = state.tab_order();
        let n = Section::ALL.len();
        assert_eq!(order[0], FocusTarget::SidebarToggle);
        for (i, section) in Section::ALL.iter().enumerate() {
            assert_eq!(order[1 + i], FocusTarget::Section(*section));
        }
        for (i, folder) in state.folders.iter().enumerate() {
            assert_eq!(order[1 + n + i], FocusTarget::Folder(folder.id));
        }
        for (i, folder) in state.folders.iter().enumerate() {
            let at = 1 + n + state.folders.len() + i;
            assert_eq!(order[at], FocusTarget::HomeFolder(folder.id));
        }
        // A preview is not connected, so "New task" is not a stop.
        assert_eq!(order.len(), 1 + n + 2 * state.folders.len());
    }

    #[test]
    fn new_task_is_the_last_stop_once_connected() {
        let mut state = ShellState::placeholder();
        state.apply(Action::SetConnectionStatus(ConnectionStatus::Connected));
        assert_eq!(state.tab_order().last(), Some(&FocusTarget::HomeStart));
    }

    #[test]
    fn a_hidden_sidebar_and_other_pages_leave_their_controls_out() {
        let mut state = ShellState::placeholder();
        state.apply(Action::ToggleSidebar);
        let order = state.tab_order();
        assert!(!order
            .iter()
            .any(|t| matches!(t, FocusTarget::Section(_) | FocusTarget::Folder(_))));
        assert_eq!(order[0], FocusTarget::SidebarToggle);
        state.apply(Action::SelectSection(Section::Approvals));
        assert_eq!(state.tab_order(), [FocusTarget::SidebarToggle]);
    }

    #[test]
    fn the_skills_page_adds_its_tabs_and_refresh_after_the_sidebar_and_only_there() {
        let mut state = ShellState::placeholder();
        let home = state.tab_order();
        assert!(!home
            .iter()
            .any(|t| matches!(t, FocusTarget::SkillsTab(_) | FocusTarget::SkillsRefresh)));
        state.apply(Action::SelectSection(Section::SkillsAndTools));
        let order = state.tab_order();
        let n = order.len();
        assert_eq!(
            order[n - 3..],
            [
                FocusTarget::SkillsTab(Tab::Skills),
                FocusTarget::SkillsTab(Tab::ConnectedTools),
                FocusTarget::SkillsRefresh
            ]
        );
        assert_eq!(order[0], FocusTarget::SidebarToggle);
        // A hidden sidebar leaves the page's own controls.
        state.apply(Action::ToggleSidebar);
        assert_eq!(state.tab_order().len(), 4);
    }

    #[test]
    fn a_shell_without_folders_tabs_through_the_toggle_and_the_sections_only() {
        assert_eq!(ShellState::new().tab_order().len(), 1 + Section::ALL.len());
    }
}
