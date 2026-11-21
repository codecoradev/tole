//! What the desktop knows about the local `tole serve` it starts or attaches
//! to (issue #1029). The supervisor in `tole-desktop` produces these values;
//! the window renders them through [`crate::strings`] and never decides on its
//! own what a failure means.

use std::time::Duration;

use crate::ConnectionStatus;

/// The state of the local serve.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ServeState {
    /// Looking for a running serve, or starting one.
    Starting,
    /// Attached to a serve that was already running (another window, or an
    /// earlier launch). Closing this desktop leaves it running.
    Attached { version: String },
    /// Running as a child this desktop started.
    Running { version: String },
    /// It stopped without being asked to. The desktop starts it again after
    /// `retry_in`; `attempt` counts the tries since it last ran steadily.
    Restarting { attempt: u32, retry_in: Duration },
    /// The desktop is stopping it ("quit completely").
    Stopping,
    /// The desktop stopped it, or never started one by choice.
    Stopped,
    /// It cannot be used. Nothing is retried while the cause stands, except
    /// after [`ServeFailure::GaveUp`], which the owner can retry.
    Failed(ServeFailure),
}

/// Why the local serve cannot be used.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ServeFailure {
    /// This platform has no local socket transport yet (Windows named pipe).
    Unsupported,
    /// No `tole` binary: not at the configured path, not next to the desktop,
    /// not on `PATH`.
    BinaryNotFound,
    /// The directory of the socket is not the owner's alone (not `0700`, owned
    /// by someone else, or a link). Nothing is sent or started there.
    DirectoryNotPrivate { path: String, reason: String },
    /// A serve answers, but speaks another protocol version (`None`: it does
    /// not say). The desktop does not talk to it; restarting it with the
    /// bundled binary fixes this.
    VersionMismatch { found: Option<u32>, expected: u32 },
    /// A serve answers, but the desktop has no credential it accepts (it was
    /// started by another tool, or its credential file is gone). Restarting it
    /// gives a new one.
    CredentialUnavailable,
    /// Something answers on the socket that is not a `tole serve`.
    NotTole,
    /// The serve did not start; `detail` is the last thing it said.
    StartFailed { detail: String },
    /// It kept stopping: `attempts` starts in a row, no steady run.
    GaveUp { attempts: u32, detail: String },
    /// "Quit completely" could not stop it.
    CouldNotStop,
}

/// What the owner can do about a failure, for the button the window shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OwnerAction {
    /// Stop the serve that is there and start the bundled one.
    RestartServe,
    /// Try to start again.
    Retry,
}

impl ServeState {
    /// Whether the desktop can use the serve now.
    pub fn is_ready(&self) -> bool {
        matches!(
            self,
            ServeState::Attached { .. } | ServeState::Running { .. }
        )
    }

    /// The version of the serve in use, when there is one.
    pub fn version(&self) -> Option<&str> {
        match self {
            ServeState::Attached { version } | ServeState::Running { version } => Some(version),
            _ => None,
        }
    }

    /// The button this state calls for, if any.
    pub fn owner_action(&self) -> Option<OwnerAction> {
        match self {
            ServeState::Failed(
                ServeFailure::VersionMismatch { .. }
                | ServeFailure::CredentialUnavailable
                | ServeFailure::NotTole,
            ) => Some(OwnerAction::RestartServe),
            ServeState::Failed(
                ServeFailure::GaveUp { .. }
                | ServeFailure::StartFailed { .. }
                | ServeFailure::CouldNotStop,
            ) => Some(OwnerAction::Retry),
            _ => None,
        }
    }

    /// The connection status of the shell for this state.
    pub fn connection_status(&self) -> ConnectionStatus {
        match self {
            ServeState::Starting | ServeState::Restarting { .. } => ConnectionStatus::Connecting,
            ServeState::Attached { .. } | ServeState::Running { .. } => ConnectionStatus::Connected,
            ServeState::Stopping | ServeState::Stopped | ServeState::Failed(_) => {
                ConnectionStatus::Unreachable
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn failed(f: ServeFailure) -> ServeState {
        ServeState::Failed(f)
    }

    #[test]
    fn only_an_attached_or_running_serve_is_ready() {
        let states = [
            (ServeState::Starting, false),
            (
                ServeState::Attached {
                    version: "1".into(),
                },
                true,
            ),
            (
                ServeState::Running {
                    version: "1".into(),
                },
                true,
            ),
            (
                ServeState::Restarting {
                    attempt: 1,
                    retry_in: Duration::from_secs(1),
                },
                false,
            ),
            (ServeState::Stopping, false),
            (ServeState::Stopped, false),
            (failed(ServeFailure::NotTole), false),
        ];
        for (state, ready) in states {
            assert_eq!(state.is_ready(), ready, "{state:?}");
            assert_eq!(state.version().is_some(), ready, "{state:?}");
            let connected = state.connection_status() == ConnectionStatus::Connected;
            assert_eq!(connected, ready, "{state:?}");
        }
    }

    #[test]
    fn a_version_mismatch_offers_a_restart_and_a_give_up_offers_a_retry() {
        let mismatch = failed(ServeFailure::VersionMismatch {
            found: Some(2),
            expected: 1,
        });
        assert_eq!(mismatch.owner_action(), Some(OwnerAction::RestartServe));
        let gave_up = failed(ServeFailure::GaveUp {
            attempts: 5,
            detail: "exit 1".into(),
        });
        assert_eq!(gave_up.owner_action(), Some(OwnerAction::Retry));
        // A missing binary or a wide directory is fixed outside the app.
        assert_eq!(failed(ServeFailure::BinaryNotFound).owner_action(), None);
        let wide = failed(ServeFailure::DirectoryNotPrivate {
            path: "/x".into(),
            reason: "mode 0755".into(),
        });
        assert_eq!(wide.owner_action(), None);
        assert_eq!(ServeState::Starting.owner_action(), None);
    }

    #[test]
    fn a_restart_in_progress_reads_as_connecting_not_as_unreachable() {
        let s = ServeState::Restarting {
            attempt: 2,
            retry_in: Duration::from_secs(2),
        };
        assert_eq!(s.connection_status(), ConnectionStatus::Connecting);
        assert_eq!(
            failed(ServeFailure::Unsupported).connection_status(),
            ConnectionStatus::Unreachable
        );
    }
}
