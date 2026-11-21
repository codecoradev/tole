//! What the offscreen render harness draws: one entry per screen and state.
//!
//! A capture names the state to render, the design board it is compared with
//! and the keys to press first (to show the keyboard focus). It holds no
//! toolkit type, so the list is tested without a window. A page PR adds the
//! captures of its screen here; `scripts/desktop-render.sh` writes one PNG per
//! capture (docs/design/desktop/README.md, "Reviewing the UI offscreen").

use crate::{Action, ConnectionStatus, Section, ShellState};

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

/// The board of the shell. The main area is still a placeholder, so only the
/// top bar, the sidebar and the connection footer can be compared.
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

/// Every state of the shell the model can produce today that is worth a look.
pub fn shell_captures() -> Vec<Capture> {
    let first_folder = ShellState::placeholder().folders[0].id;
    let mut real = ShellState::new();
    real.apply(Action::AddFolder {
        name: "Contracts".to_owned(),
        path: "~/Contracts".to_owned(),
    });
    real.apply(Action::SetConnectionStatus(ConnectionStatus::Connected));
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
            preview_with([Action::SetApprovalsPending(3)]),
        ),
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
        capture(
            "shell-connected",
            HOME,
            "",
            status(ConnectionStatus::Connected),
        ),
        capture(
            "shell-unreachable",
            HOME,
            "",
            status(ConnectionStatus::Unreachable),
        ),
        capture("shell-no-folders", HOME, "", ShellState::new()),
        capture("shell-real-folder", HOME, "", real),
        capture("shell-focus-first-section", HOME, "tab", preview()),
        capture(
            "shell-focus-first-folder",
            HOME,
            "tab tab tab tab tab tab tab",
            preview(),
        ),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
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
    fn the_focus_captures_press_enough_tabs_to_reach_their_row() {
        let captures = shell_captures();
        let by_name = |n: &str| captures.iter().find(|c| c.name == n).expect("capture");
        // Tab visits the six sections, then the folders: the 7th stop is the first folder.
        assert_eq!(
            by_name("shell-focus-first-section")
                .keys
                .split_whitespace()
                .count(),
            1
        );
        assert_eq!(
            by_name("shell-focus-first-folder")
                .keys
                .split_whitespace()
                .count(),
            Section::ALL.len() + 1
        );
    }
}
