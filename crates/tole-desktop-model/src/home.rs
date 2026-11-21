//! The home page (design board S2): the empty state with the working folders
//! and the "New task" button. Derived from [`ShellState`]; the composer, "Pick
//! up where you left off" and "Coming up" need the client (#1040, #1042) and
//! come with it.

use crate::{ConnectionStatus, FolderId, ShellState};

/// A folder the owner can start a task in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HomeFolder {
    pub id: FolderId,
    pub name: String,
    pub path: String,
    pub selected: bool,
}

/// What the home page shows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Home {
    /// The folders, in sidebar order. Empty: the page says there are none.
    pub folders: Vec<HomeFolder>,
    /// Whether "New task" can be pressed: only while connected, because a
    /// task is a session on a serve. Until then the page says why.
    pub can_start: bool,
}

impl ShellState {
    /// The home page for the current state.
    pub fn home(&self) -> Home {
        Home {
            folders: self
                .folders
                .iter()
                .map(|f| HomeFolder {
                    id: f.id,
                    name: f.name.clone(),
                    path: f.path.clone(),
                    selected: self.selected_folder == Some(f.id),
                })
                .collect(),
            can_start: self.connection.status == ConnectionStatus::Connected,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Action;

    #[test]
    fn a_remote_server_never_reads_as_connected_whoever_sets_it() {
        let mut shell = ShellState::new();
        let remote = || crate::Connection {
            target: crate::Target::Remote {
                name: "Office".into(),
            },
            status: ConnectionStatus::Connected,
        };
        shell.apply(Action::SetConnection(remote()));
        assert_eq!(shell.connection.status, ConnectionStatus::Connecting);
        assert!(!shell.home().can_start);
        shell.apply(Action::SetConnectionStatus(ConnectionStatus::Connected));
        assert_eq!(shell.connection.status, ConnectionStatus::Connecting);
        assert!(!shell.home().can_start);
    }

    #[test]
    fn a_new_shell_has_an_empty_home_that_cannot_start_yet() {
        let home = ShellState::new().home();
        assert!(home.folders.is_empty());
        assert!(!home.can_start);
    }

    #[test]
    fn the_home_lists_the_folders_in_order_and_marks_the_selected_one() {
        let mut shell = ShellState::placeholder();
        let names = |h: &Home| h.folders.iter().map(|f| f.name.clone()).collect::<Vec<_>>();
        assert_eq!(names(&shell.home()), ["Invoices", "Reports", "Photos"]);
        assert!(shell.home().folders.iter().all(|f| !f.selected));
        let id = shell.folders[2].id;
        shell.apply(Action::SelectFolder(Some(id)));
        let home = shell.home();
        assert_eq!(
            home.folders.iter().filter(|f| f.selected).count(),
            1,
            "exactly one folder is selected"
        );
        assert!(home.folders[2].selected);
    }

    #[test]
    fn only_a_connected_shell_can_start_a_task() {
        let mut shell = ShellState::placeholder();
        for (status, can) in [
            (ConnectionStatus::Preview, false),
            (ConnectionStatus::Connecting, false),
            (ConnectionStatus::Unreachable, false),
            (ConnectionStatus::Connected, true),
        ] {
            shell.apply(Action::SetConnectionStatus(status));
            assert_eq!(shell.home().can_start, can, "{status:?}");
        }
    }
}
