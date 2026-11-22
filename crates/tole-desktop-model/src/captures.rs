//! What the offscreen render harness draws: one entry per screen and state.
//!
//! A capture names the state to render, the design board it is compared with
//! and the keys to press first (to show the keyboard focus). It holds no
//! toolkit type, so the list is tested without a window. A page PR adds the
//! captures of its screen here; `scripts/desktop-render.sh` writes one PNG per
//! capture (docs/design/desktop/README.md, "Reviewing the UI offscreen").

use crate::skills::Badge;
use crate::{Action, Connection, ConnectionStatus, Section, ShellState, Target};

/// One image to render.
#[derive(Debug, Clone)]
pub struct Capture {
    /// File stem of the PNG: lower case, digits and `-`.
    pub name: &'static str,
    /// The design board (a file in docs/design/desktop) to compare it with.
    pub board: &'static str,
    /// Space separated keystrokes pressed before the image is taken
    /// (`"tab"`, `"shift-tab"`); empty for none.
    pub keys: &'static str,
    pub state: ShellState,
}

/// The board of the shell and of the home page (the composer, "Pick up where
/// you left off" and "Coming up" are not built yet: compare the header, the
/// sidebar, the counts, the folders and the connection footer).
const HOME: &str = "s2-home.png";

fn capture(
    name: &'static str,
    board: &'static str,
    keys: &'static str,
    state: ShellState,
) -> Capture {
    Capture {
        name,
        board,
        keys,
        state,
    }
}

/// The preview state with `actions` applied.
fn preview_with(actions: impl IntoIterator<Item = Action>) -> ShellState {
    let mut state = ShellState::placeholder();
    for action in actions {
        state.apply(action);
    }
    state
}

fn section(section: Section) -> ShellState {
    preview_with([Action::SelectSection(section)])
}

fn status(status: ConnectionStatus) -> ShellState {
    preview_with([Action::SetConnectionStatus(status)])
}

/// The counts of board S2: 1 approval, 2 automations on, 1 discussion that
/// needs the owner, 2 new skills and tools.
fn counts() -> ShellState {
    preview_with([
        Action::SetApprovalsPending(Badge::Count(1)),
        Action::SetAutomationsOn(2),
        Action::SetDiscussionsNeedYou(1),
        Action::SetSkillsNew(2),
    ])
}

fn connected() -> ShellState {
    status(ConnectionStatus::Connected)
}

/// A real folder and a remote server, both with names longer than the sidebar
/// and the header can show: they must be cut, never overflow.
fn long_names() -> ShellState {
    let mut state = ShellState::new();
    state.apply(Action::AddFolder {
        name: "Quarterly invoices and receipts for the whole accounting department".to_owned(),
        path: "~/Accounting/Quarterly".to_owned(),
    });
    state.apply(Action::SetConnection(Connection {
        target: Target::Remote {
            name: "Head office server in the main building, second floor".to_owned(),
        },
        status: ConnectionStatus::Connecting,
    }));
    state.apply(Action::SetApprovalsPending(Badge::Count(12)));
    state.apply(Action::SetDiscussionsNeedYou(3));
    state
}

/// Every state of the shell the model can produce today that is worth a look.
pub fn shell_captures() -> Vec<Capture> {
    let first_folder = ShellState::placeholder().folders[0].id;
    let mut real = ShellState::new();
    real.apply(Action::AddFolder {
        name: "Contracts".to_owned(),
        path: "~/Contracts".to_owned(),
    });
    real.apply(Action::SetConnectionStatus(ConnectionStatus::Connected));
    real.apply(Action::SetApprovalsPending(Badge::Count(0)));
    let mut empty_connected = ShellState::new();
    empty_connected.apply(Action::SetConnectionStatus(ConnectionStatus::Connected));
    empty_connected.apply(Action::SetApprovalsPending(Badge::Count(0)));
    let mut no_folders = ShellState::new();
    no_folders.apply(Action::SetApprovalsPending(Badge::Count(0)));
    let preview = ShellState::placeholder;
    vec![
        capture("shell-preview", HOME, "", preview()),
        capture(
            "shell-folder-selected",
            HOME,
            "",
            preview_with([Action::SelectFolder(Some(first_folder))]),
        ),
        capture(
            "shell-approvals-pending",
            "s5-approvals.png",
            "",
            preview_with([Action::SetApprovalsPending(Badge::Count(3))]),
        ),
        capture(
            "shell-approvals-unknown",
            HOME,
            "",
            preview_with([Action::SetApprovalsPending(Badge::Unknown)]),
        ),
        capture("shell-counts", HOME, "", counts()),
        capture(
            "shell-sidebar-collapsed",
            HOME,
            "",
            preview_with([Action::ToggleSidebar]),
        ),
        capture(
            "shell-remote-server",
            HOME,
            "",
            preview_with([Action::SetConnection(Connection {
                target: Target::Remote {
                    name: "Office server".to_owned(),
                },
                status: ConnectionStatus::Connecting,
            })]),
        ),
        capture("shell-long-names", HOME, "", long_names()),
        capture("shell-section-search", HOME, "", section(Section::Search)),
        capture(
            "shell-section-approvals",
            "s5-approvals.png",
            "",
            section(Section::Approvals),
        ),
        capture(
            "shell-section-automations",
            "s6-automations.png",
            "",
            section(Section::Automations),
        ),
        capture(
            "shell-section-discussions",
            "s15-discussions.png",
            "",
            section(Section::Discussions),
        ),
        capture(
            "shell-section-skills-and-tools",
            "s7-skills.png",
            "",
            section(Section::SkillsAndTools),
        ),
        capture(
            "shell-connecting",
            HOME,
            "",
            status(ConnectionStatus::Connecting),
        ),
        capture("shell-connected", HOME, "", connected()),
        capture(
            "shell-unreachable",
            HOME,
            "",
            status(ConnectionStatus::Unreachable),
        ),
        capture("shell-no-folders", HOME, "", no_folders),
        capture("shell-no-folders-connected", HOME, "", empty_connected),
        capture("shell-real-folder", HOME, "", real),
        capture("shell-focus-sidebar-toggle", HOME, "tab", preview()),
        capture("shell-focus-first-section", HOME, "tab tab", preview()),
        capture(
            "shell-focus-first-folder",
            HOME,
            "tab tab tab tab tab tab tab tab",
            preview(),
        ),
        capture(
            "shell-focus-home-folder",
            HOME,
            "tab tab tab tab tab tab tab tab tab tab tab",
            preview(),
        ),
        capture(
            "shell-focus-new-task",
            HOME,
            "tab tab tab tab tab tab tab tab tab tab tab tab tab tab",
            connected(),
        ),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::FocusTarget;
    use std::collections::HashSet;

    #[test]
    fn names_are_unique_and_safe_as_file_stems() {
        let captures = shell_captures();
        let names: HashSet<_> = captures.iter().map(|c| c.name).collect();
        assert_eq!(names.len(), captures.len());
        for c in &captures {
            assert!(!c.name.is_empty());
            assert!(
                c.name
                    .chars()
                    .all(|ch| ch.is_ascii_lowercase() || ch.is_ascii_digit() || ch == '-'),
                "{}",
                c.name
            );
        }
    }

    #[test]
    fn the_preview_with_the_sample_folders_is_the_first_capture() {
        let first = &shell_captures()[0];
        assert_eq!(first.name, "shell-preview");
        assert_eq!(first.state, ShellState::placeholder());
        assert!(first.state.sample_data());
        assert_eq!(first.state.connection.status, ConnectionStatus::Preview);
    }

    #[test]
    fn every_section_and_connection_status_has_a_capture() {
        let captures = shell_captures();
        for section in Section::ALL.into_iter().filter(|s| *s != Section::NewTask) {
            assert!(captures.iter().any(|c| c.state.section == section));
        }
        for status in [
            ConnectionStatus::Preview,
            ConnectionStatus::Connecting,
            ConnectionStatus::Connected,
            ConnectionStatus::Unreachable,
        ] {
            assert!(captures.iter().any(|c| c.state.connection.status == status));
        }
    }

    #[test]
    fn boards_are_png_files_and_keys_are_known_keystrokes() {
        for c in shell_captures() {
            assert!(c.board.ends_with(".png"), "{}", c.board);
            for key in c.keys.split_whitespace() {
                assert!(matches!(key, "tab" | "shift-tab"), "{key}");
            }
        }
    }

    #[test]
    fn each_focus_capture_presses_exactly_the_tabs_that_reach_its_control() {
        let captures = shell_captures();
        let first_folder = ShellState::placeholder().folders[0].id;
        for (name, target) in [
            ("shell-focus-sidebar-toggle", FocusTarget::SidebarToggle),
            (
                "shell-focus-first-section",
                FocusTarget::Section(Section::NewTask),
            ),
            (
                "shell-focus-first-folder",
                FocusTarget::Folder(first_folder),
            ),
            (
                "shell-focus-home-folder",
                FocusTarget::HomeFolder(first_folder),
            ),
            ("shell-focus-new-task", FocusTarget::HomeStart),
        ] {
            let c = captures.iter().find(|c| c.name == name).expect(name);
            let presses = c.keys.split_whitespace().count();
            assert_eq!(
                c.state.tab_order().get(presses - 1),
                Some(&target),
                "{name}: {presses} tabs"
            );
        }
    }

    #[test]
    fn every_count_the_shell_can_show_has_a_capture_and_zero_hides_it() {
        let captures = shell_captures();
        let counted = [
            Section::Approvals,
            Section::Automations,
            Section::Discussions,
            Section::SkillsAndTools,
        ];
        assert!(captures
            .iter()
            .any(|c| counted.iter().all(|s| c.state.badge(*s).is_some())));
        let preview = &captures[0];
        assert!(counted.iter().all(|s| preview.state.badge(*s).is_none()));
    }

    #[test]
    fn the_header_states_are_captured() {
        let captures = shell_captures();
        assert!(captures.iter().any(|c| c.state.sidebar_collapsed));
        assert!(captures
            .iter()
            .any(|c| matches!(c.state.connection.target, Target::Remote { .. })));
        // A name longer than the sidebar can hold, to see it cut.
        assert!(captures.iter().any(|c| c
            .state
            .folders
            .iter()
            .any(|f| f.name.chars().count() > 40)));
        // The home page with no folder, and one that can start a task.
        assert!(captures
            .iter()
            .any(|c| c.state.folders.is_empty() && c.state.home().can_start));
        assert!(captures.iter().any(|c| c.state.home().can_start));
    }
}
