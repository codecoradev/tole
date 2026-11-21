//! The string table: every word the window shows.
//!
//! One module so wording is reviewed in one place and a later translation has
//! one file to start from. The UI layer reads these; it holds no copy of its
//! own. Wording follows the design draft (plain words for people who are not
//! technical) and is still a draft.

use crate::{ConnectionStatus, ConnectionsError, EndpointError, PlainHttp, Section, Target};

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

pub const HOME_TITLE: &str = "What would you like Tole to do?";
pub const HOME_SUBTITLE: &str =
    "Describe it in your own words. Tole shows its plan and asks before it changes anything.";

/// Where the desktop is connected.
pub fn target_label(target: &Target) -> &'static str {
    match target {
        Target::ThisComputer => "This computer",
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
}
