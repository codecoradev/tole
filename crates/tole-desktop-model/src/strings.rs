//! The string table: every word the window shows.
//!
//! One module so wording is reviewed in one place and a later translation has
//! one file to start from. The UI layer reads these; it holds no copy of its
//! own. Wording follows the design draft (plain words for people who are not
//! technical) and is still a draft.

use crate::{
    ConnectionStatus, ConnectionsError, EndpointError, OwnerAction, PlainHttp, Section,
    ServeFailure, ServeState, Target,
};

pub use crate::approvals::strings as approvals;

pub const APP_NAME: &str = "Tole";
pub const WINDOW_TITLE: &str = "Tole";

/// The sidebar label of a section.
pub fn section_label(section: Section) -> &'static str {
    match section {
        Section::NewTask => "New task",
        Section::Search => "Search",
        Section::Approvals => "Approvals",
        Section::Automations => "Automations",
        Section::Discussions => "Discussions",
        Section::SkillsAndTools => "Skills and tools",
    }
}

/// Shown by a build without the `gpui` feature (a developer message, not UI).
pub const NO_WINDOW_TOOLKIT: &str = "this build has no window toolkit; rebuild with `--features gpui` (see docs/design/desktop/README.md)";

/// Prefix of the error when the window cannot open (a developer message).
pub const WINDOW_FAILED: &str = "cannot open the window";

pub const BREADCRUMB_SEPARATOR: &str = "/";
/// Shown on a page that is not built yet.
pub const SECTION_PLACEHOLDER: &str = "This page is not built yet.";

pub const FOLDERS_CAPTION: &str = "FOLDERS";
/// Shown under the folders caption while they are sample data.
pub const FOLDERS_SAMPLE_NOTE: &str = "Sample folders (preview)";
pub const FOLDERS_EMPTY: &str = "No folders yet";

/// The name of the header button while the sidebar is shown (what a press does).
pub const SIDEBAR_HIDE: &str = "Hide the sidebar";
/// The name of the header button while the sidebar is hidden.
pub const SIDEBAR_SHOW: &str = "Show the sidebar";

/// The count beside a sidebar row, in words: "3" for approvals, then "2 on",
/// "1 needs you" and "2 new" as in board S2. Sections without a count get the
/// bare number.
pub fn badge_label(section: Section, count: usize) -> String {
    match section {
        Section::Automations => format!("{count} on"),
        Section::Discussions => format!("{count} needs you"),
        Section::SkillsAndTools => format!("{count} new"),
        _ => count.to_string(),
    }
}

/// Caption above the folder chips of the home page.
pub const HOME_FOLDERS_CAPTION: &str = "Working folder";
/// Home page, no folders yet.
pub const HOME_FOLDERS_EMPTY: &str =
    "You have no working folders yet. Add one to give Tole a place to work.";
/// The primary button of the home page.
pub const HOME_START: &str = "New task";
/// Under the button while it cannot be pressed.
pub const HOME_START_BLOCKED: &str = "Tole is not connected yet, so a task cannot start.";

pub const HOME_TITLE: &str = "What would you like Tole to do?";
pub const HOME_SUBTITLE: &str =
    "Describe it in your own words. Tole shows its plan and asks before it changes anything.";

/// Where the desktop is connected: this computer, or the remote server by the
/// name the owner gave it.
pub fn target_label(target: &Target) -> String {
    match target {
        Target::ThisComputer => "This computer".to_owned(),
        Target::Remote { name } => format!("{name} (remote server)"),
    }
}

/// How the connection is going, shown next to [`target_label`].
pub fn status_label(status: ConnectionStatus) -> &'static str {
    match status {
        ConnectionStatus::Preview => "Not connected (preview)",
        ConnectionStatus::Connecting => "connecting",
        ConnectionStatus::Connected => "connected",
        ConnectionStatus::Unreachable => "not reachable",
    }
}

/// What the local serve is doing, for the connection line and the state page.
pub fn serve_state_label(state: &ServeState) -> String {
    match state {
        ServeState::Starting => "Starting Tole in the background".to_owned(),
        ServeState::Attached { version } => format!("Connected to Tole {version} on this computer"),
        ServeState::Running { version } => format!("Tole {version} is running on this computer"),
        ServeState::Restarting { attempt, retry_in } => format!(
            "Tole stopped. Starting it again in {} s (try {attempt})",
            retry_in.as_secs().max(1)
        ),
        ServeState::Stopping => "Stopping Tole".to_owned(),
        ServeState::Stopped => "Tole is stopped".to_owned(),
        ServeState::Failed(failure) => serve_failure_text(failure),
    }
}

/// Why the local serve cannot be used, in words for the owner.
pub fn serve_failure_text(failure: &ServeFailure) -> String {
    match failure {
        ServeFailure::Unsupported => {
            "This computer cannot run Tole in the background from the desktop yet.".to_owned()
        }
        ServeFailure::BinaryNotFound => {
            "The desktop cannot find the tole program. Install it, or set its location in Settings."
                .to_owned()
        }
        ServeFailure::DirectoryNotPrivate { path, reason } => format!(
            "The folder {path} is not private to you ({reason}), so the desktop will not use it. \
             Make it yours alone and try again."
        ),
        ServeFailure::VersionMismatch { found, expected } => {
            let seen = match found {
                Some(found) => format!("speaks version {found}"),
                None => "is too old to say which version it speaks".to_owned(),
            };
            format!(
                "The Tole running here {seen}; this desktop needs version {expected}. \
                 Restart it with the one that came with the desktop."
            )
        }
        ServeFailure::CredentialUnavailable => {
            "Tole is running here, but it was started by something else and the desktop cannot \
             sign in to it. Restart it from the desktop."
                .to_owned()
        }
        ServeFailure::NotTole => {
            "Something that is not Tole answers where the desktop expects it. Restart it from \
             the desktop."
                .to_owned()
        }
        ServeFailure::StartFailed { detail } => format!("Tole did not start: {detail}"),
        ServeFailure::GaveUp { attempts, detail } => {
            format!("Tole stopped {attempts} times in a row and the desktop gave up: {detail}")
        }
        ServeFailure::CouldNotStop => {
            "Tole did not stop when asked. It may still be running.".to_owned()
        }
    }
}

/// The button label a failure calls for.
pub fn owner_action_label(action: OwnerAction) -> &'static str {
    match action {
        OwnerAction::RestartServe => "Restart Tole",
        OwnerAction::Retry => "Try again",
    }
}

/// Why a server address was refused, in words for the owner.
pub fn endpoint_error_text(error: &EndpointError) -> String {
    match error {
        EndpointError::Empty => "Enter the address of the server.".to_owned(),
        EndpointError::Scheme => {
            "The address must start with https:// (or http:// for a trusted private network)."
                .to_owned()
        }
        EndpointError::CredentialInUrl => {
            "Leave the password or token out of the address. Enter it in the credential field."
                .to_owned()
        }
        EndpointError::QueryOrFragment => "The address of a server has no ? or # part.".to_owned(),
        EndpointError::Host => {
            "The address needs a server name or an IP address, for example https://tole.example.com."
                .to_owned()
        }
        EndpointError::Port => "The port must be a number from 1 to 65535.".to_owned(),
        EndpointError::Path => {
            "The address has a space or a control character in its path.".to_owned()
        }
        EndpointError::PlainHttp {
            host,
            why: PlainHttp::NotTrusted,
        } => format!(
            "http:// sends your credential in the clear, so it is only for this computer. For \
             {host}, use https://, or mark it as a trusted private network (a VPN or a network \
             you control)."
        ),
        EndpointError::PlainHttp {
            host,
            why: PlainHttp::PublicAddress,
        } => format!(
            "{host} is a public address. http:// is never used with one, even when marked \
             trusted. Use https://."
        ),
    }
}

/// Why a server entry was refused.
pub fn connections_error_text(error: &ConnectionsError) -> String {
    match error {
        ConnectionsError::RemoteDisabled => "Remote servers are turned off in Settings.".to_owned(),
        ConnectionsError::NameEmpty => "Give the server a name.".to_owned(),
        ConnectionsError::NameTooLong => {
            "The name is too long or has a control character.".to_owned()
        }
        ConnectionsError::NameTaken => "Another server has this name.".to_owned(),
        ConnectionsError::TooMany => "There are too many servers. Remove one first.".to_owned(),
        ConnectionsError::UnknownRemote => "That server is no longer in the list.".to_owned(),
        ConnectionsError::Endpoint(e) => endpoint_error_text(e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_http_refusal_says_why_and_what_to_do() {
        let untrusted = endpoint_error_text(&EndpointError::PlainHttp {
            host: "10.0.0.5".into(),
            why: PlainHttp::NotTrusted,
        });
        assert!(untrusted.contains("10.0.0.5") && untrusted.contains("https://"));
        assert!(untrusted.contains("trusted private network"));
        let public = endpoint_error_text(&EndpointError::PlainHttp {
            host: "203.0.113.9".into(),
            why: PlainHttp::PublicAddress,
        });
        assert!(public.contains("public") && !public.contains("mark it"));
    }

    #[test]
    fn every_serve_state_has_a_label_and_a_failure_names_its_cause() {
        let attached = serve_state_label(&ServeState::Attached {
            version: "0.12.0".into(),
        });
        assert!(attached.contains("0.12.0"));
        let mismatch = serve_failure_text(&ServeFailure::VersionMismatch {
            found: Some(2),
            expected: 1,
        });
        assert!(mismatch.contains('2') && mismatch.contains('1') && mismatch.contains("Restart"));
        assert!(!serve_state_label(&ServeState::Stopped).is_empty());
        assert_ne!(
            owner_action_label(OwnerAction::Retry),
            owner_action_label(OwnerAction::RestartServe)
        );
    }

    #[test]
    fn every_section_has_a_distinct_nonempty_label() {
        let labels: Vec<_> = Section::ALL.iter().map(|s| section_label(*s)).collect();
        for (i, a) in labels.iter().enumerate() {
            assert!(!a.is_empty());
            assert!(labels[i + 1..].iter().all(|b| a != b));
        }
    }

    #[test]
    fn the_preview_status_says_it_is_not_connected() {
        let label = status_label(ConnectionStatus::Preview);
        assert!(label.contains("Not connected") && label.contains("preview"));
        assert_ne!(label, status_label(ConnectionStatus::Connected));
    }

    #[test]
    fn this_computer_wording_is_the_design_wording() {
        assert_eq!(target_label(&Target::ThisComputer), "This computer");
    }

    #[test]
    fn a_remote_server_is_labelled_by_its_name() {
        let label = target_label(&Target::Remote {
            name: "Office".to_owned(),
        });
        assert!(label.contains("Office") && label.contains("remote"));
    }

    #[test]
    fn counts_read_as_on_the_board() {
        assert_eq!(badge_label(Section::Approvals, 1), "1");
        assert_eq!(badge_label(Section::Automations, 2), "2 on");
        assert_eq!(badge_label(Section::Discussions, 1), "1 needs you");
        assert_eq!(badge_label(Section::SkillsAndTools, 2), "2 new");
    }
}

pub mod automations;
