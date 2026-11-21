//! Asking a socket whether a `tole serve` answers on it, and whether the
//! desktop may talk to it: the protocol version must match, and the credential
//! it holds must be accepted.

use std::path::Path;
use std::time::Duration;

use tole_client::{Client, Error};
use tole_desktop_model::ServeFailure;

/// The `tole serve` protocol this desktop speaks (`protocol` in `GET /health`;
/// `SERVE_PROTOCOL_VERSION` in `tole-cli`, which the desktop does not depend
/// on: a test of the real binary keeps the two equal). A serve that reports
/// another value, or none, is not talked to.
pub const EXPECTED_PROTOCOL: u32 = 1;

/// How long a probe waits for an answer.
const PROBE_TIMEOUT: Duration = Duration::from_secs(2);

/// What is on the socket.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Probe {
    /// Nothing listens: no socket file, a stale one, or a refused connection.
    Absent,
    /// Something answers `GET /health` with `status: ok`.
    Serve {
        protocol: Option<u32>,
        version: Option<String>,
    },
    /// Something is there that does not answer like a serve.
    NotTole,
    /// The client refused to use the socket because its directory or file is not
    /// the owner's alone: nothing was sent. The text says why.
    Insecure(String),
    /// The socket path cannot be used at all (too long for a socket address).
    Invalid(String),
}

/// A client for the socket, with the answer bounded.
pub fn client(socket: &Path, token: Option<&str>) -> Result<Client, Error> {
    let client = Client::connect_unix(socket, token.map(str::to_owned))?;
    client.with_request_timeout(PROBE_TIMEOUT)
}

pub fn probe(socket: &Path) -> Probe {
    let client = match client(socket, None) {
        Ok(c) => c,
        Err(Error::Connect(_)) => return Probe::Absent,
        Err(Error::InsecureSocket { reason, .. }) => return Probe::Insecure(reason),
        Err(e) => return Probe::Invalid(e.to_string()),
    };
    match client.health() {
        Ok(h) if h.status == "ok" => Probe::Serve {
            protocol: h.protocol,
            version: h.version,
        },
        Ok(_) => Probe::NotTole,
        Err(Error::Connect(_)) => Probe::Absent,
        Err(Error::InsecureSocket { reason, .. }) => Probe::Insecure(reason),
        Err(_) => Probe::NotTole,
    }
}

/// The version of a serve whose protocol matches, or the failure that says
/// it does not.
pub fn check_protocol(
    protocol: Option<u32>,
    version: Option<&str>,
) -> Result<String, ServeFailure> {
    if protocol == Some(EXPECTED_PROTOCOL) {
        Ok(version.unwrap_or("unknown").to_owned())
    } else {
        Err(ServeFailure::VersionMismatch {
            found: protocol,
            expected: EXPECTED_PROTOCOL,
        })
    }
}

/// Whether the serve accepts `token`: an authenticated route answers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Credential {
    Accepted,
    Rejected,
    /// The serve did not answer in time or at all; nothing is known.
    Unknown,
}

pub fn check_credential(socket: &Path, token: &str) -> Credential {
    let Ok(client) = client(socket, Some(token)) else {
        return Credential::Unknown;
    };
    match client.sessions() {
        Ok(_) => Credential::Accepted,
        Err(Error::Status {
            status: 401 | 403, ..
        }) => Credential::Rejected,
        Err(_) => Credential::Unknown,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_expected_protocol_is_accepted_and_the_failure_says_what_was_found() {
        assert_eq!(
            check_protocol(Some(EXPECTED_PROTOCOL), Some("0.12.0")).unwrap(),
            "0.12.0"
        );
        assert_eq!(
            check_protocol(Some(EXPECTED_PROTOCOL), None).unwrap(),
            "unknown"
        );
        for found in [Some(0), Some(EXPECTED_PROTOCOL + 1), None] {
            assert_eq!(
                check_protocol(found, Some("9.9.9")).unwrap_err(),
                ServeFailure::VersionMismatch {
                    found,
                    expected: EXPECTED_PROTOCOL
                }
            );
        }
    }

    #[test]
    fn a_missing_socket_is_absent() {
        let dir = std::env::temp_dir().join(format!("tdprobe-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::set_permissions(&dir, std::os::unix::fs::PermissionsExt::from_mode(0o700))
            .unwrap();
        assert_eq!(probe(&dir.join("none.sock")), Probe::Absent);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
