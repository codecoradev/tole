//! The header of the shell (design board S2): the sidebar toggle, the
//! breadcrumb and the connection indicator. All of it is derived from
//! [`ShellState`], so a test needs no window.

use crate::tokens::Tone;
use crate::{
    strings, Connection, ConnectionId, ConnectionStatus, Connections, Section, ServeState,
    ShellState, Target,
};

/// Where the desktop is connected and how far that is, as the header and the
/// sidebar footer show it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Indicator {
    /// "This computer", or the name of the remote server.
    pub target: String,
    /// "connecting", "Not connected (preview)", ...
    pub status: &'static str,
    /// Colour meaning of the dot.
    pub tone: Tone,
}

impl Connection {
    /// The connection the shell shows for the selected entry of `connections`.
    ///
    /// This computer reads as the local serve says
    /// ([`ServeState::connection_status`]). A remote server has no health
    /// report until the remote client exists (#1044), so it reads as
    /// connecting and never as connected: the window does not claim a link it
    /// has not seen.
    pub fn resolve(connections: &Connections, serve: &ServeState) -> Connection {
        match connections.selected() {
            ConnectionId::ThisComputer => Connection {
                target: Target::ThisComputer,
                status: serve.connection_status(),
            },
            ConnectionId::Remote(id) => match connections.remote(id) {
                Some(remote) => Connection {
                    target: Target::Remote {
                        name: remote.name.clone(),
                    },
                    status: ConnectionStatus::Connecting,
                },
                // The entry went away under the selection: fall back to the
                // local serve rather than naming a server that is not there.
                None => Connection {
                    target: Target::ThisComputer,
                    status: serve.connection_status(),
                },
            },
        }
    }
}

impl ShellState {
    /// Whether the sidebar is shown.
    pub fn sidebar_visible(&self) -> bool {
        !self.sidebar_collapsed
    }

    /// The name of the sidebar toggle: it says what a press does. The toggle
    /// is an icon, so this is also its name for a screen reader.
    pub fn sidebar_toggle_label(&self) -> &'static str {
        if self.sidebar_visible() {
            strings::SIDEBAR_HIDE
        } else {
            strings::SIDEBAR_SHOW
        }
    }

    /// The path shown in the header: the app, then the folder when one is
    /// selected on the home page, otherwise the section.
    pub fn breadcrumb(&self) -> Vec<String> {
        let place = match (self.section, self.selected()) {
            (Section::NewTask, Some(folder)) => folder.name.clone(),
            (section, _) => strings::section_label(section).to_owned(),
        };
        vec![strings::APP_NAME.to_owned(), place]
    }

    /// The connection indicator.
    pub fn connection_indicator(&self) -> Indicator {
        Indicator {
            target: strings::target_label(&self.connection.target),
            status: strings::status_label(self.connection.status),
            tone: self.connection.status.tone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Action, RemoteDraft};

    fn with_remote() -> Connections {
        let mut connections = Connections::new();
        connections.set_remote_enabled(true);
        let saved = connections
            .save_remote(RemoteDraft {
                name: "Office server".to_owned(),
                url: "https://tole.example.com".to_owned(),
                trusted_private_network: false,
                credential: None,
            })
            .expect("a valid remote");
        connections
            .select(ConnectionId::Remote(saved.id))
            .expect("select");
        connections
    }

    #[test]
    fn this_computer_follows_the_local_serve() {
        let connections = Connections::new();
        for (serve, status) in [
            (ServeState::Starting, ConnectionStatus::Connecting),
            (
                ServeState::Running {
                    version: "1".into(),
                },
                ConnectionStatus::Connected,
            ),
            (ServeState::Stopped, ConnectionStatus::Unreachable),
        ] {
            let c = Connection::resolve(&connections, &serve);
            assert_eq!(c.target, Target::ThisComputer);
            assert_eq!(c.status, status);
        }
    }

    #[test]
    fn a_remote_server_is_named_and_never_reads_as_connected() {
        let connections = with_remote();
        let serve = ServeState::Running {
            version: "1".into(),
        };
        let c = Connection::resolve(&connections, &serve);
        assert_eq!(
            c.target,
            Target::Remote {
                name: "Office server".to_owned()
            }
        );
        assert_ne!(c.status, ConnectionStatus::Connected);
        let mut shell = ShellState::new();
        shell.apply(Action::SetConnection(c));
        let indicator = shell.connection_indicator();
        assert!(indicator.target.contains("Office server"));
        assert_ne!(
            indicator.target,
            strings::target_label(&Target::ThisComputer)
        );
    }

    #[test]
    fn the_indicator_says_this_computer_and_the_status_word_with_its_tone() {
        let shell = ShellState::placeholder();
        let indicator = shell.connection_indicator();
        assert_eq!(indicator.target, "This computer");
        assert_eq!(
            indicator.status,
            strings::status_label(ConnectionStatus::Preview)
        );
        assert_eq!(indicator.tone, Tone::Neutral);
    }

    #[test]
    fn the_breadcrumb_names_the_section_or_the_selected_folder() {
        let mut shell = ShellState::placeholder();
        assert_eq!(shell.breadcrumb(), ["Tole", "New task"]);
        let id = shell.folders[1].id;
        shell.apply(Action::SelectFolder(Some(id)));
        assert_eq!(shell.breadcrumb(), ["Tole", "Reports"]);
        shell.apply(Action::SelectSection(Section::Approvals));
        assert_eq!(shell.breadcrumb(), ["Tole", "Approvals"]);
    }

    #[test]
    fn the_toggle_says_what_a_press_does() {
        let mut shell = ShellState::new();
        assert!(shell.sidebar_visible());
        let hide = shell.sidebar_toggle_label();
        shell.apply(Action::ToggleSidebar);
        assert!(!shell.sidebar_visible());
        assert_ne!(shell.sidebar_toggle_label(), hide);
    }
}
