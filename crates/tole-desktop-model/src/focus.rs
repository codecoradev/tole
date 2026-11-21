//! Keyboard focus order of the app shell.
//!
//! The order is part of the model, so a test can check what the window really
//! does against it: Tab moves through the sidebar sections in sidebar order,
//! then through the folders in list order.

use crate::{FolderId, Section, ShellState};

/// A row of the shell that can take keyboard focus.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FocusTarget {
    Section(Section),
    Folder(FolderId),
}

impl ShellState {
    /// The rows Tab visits, first to last.
    pub fn tab_order(&self) -> Vec<FocusTarget> {
        Section::ALL
            .iter()
            .map(|s| FocusTarget::Section(*s))
            .chain(self.folders.iter().map(|f| FocusTarget::Folder(f.id)))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sections_come_first_in_sidebar_order_then_the_folders() {
        let state = ShellState::placeholder();
        let order = state.tab_order();
        assert_eq!(order.len(), Section::ALL.len() + state.folders.len());
        for (i, section) in Section::ALL.iter().enumerate() {
            assert_eq!(order[i], FocusTarget::Section(*section));
        }
        for (i, folder) in state.folders.iter().enumerate() {
            assert_eq!(
                order[Section::ALL.len() + i],
                FocusTarget::Folder(folder.id)
            );
        }
    }

    #[test]
    fn a_shell_without_folders_tabs_through_the_sections_only() {
        assert_eq!(ShellState::new().tab_order().len(), Section::ALL.len());
    }
}
