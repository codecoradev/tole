//! The one error type of the client.

use serde_json::Value;

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The connection could not be opened (name resolution, refused, timed
    /// out). Nothing was sent, so retrying is always safe.
    #[error("could not connect: {0}")]
    Connect(std::io::Error),
    /// The connection failed or dropped after it was open. The request may
    /// or may not have reached the server. Errors that are really a wire
    /// format violation or an unusable setting are not `Io`: see the
    /// `From<io::Error>` conversion.
    #[error("i/o error: {0}")]
    Io(std::io::Error),
    /// The server url is not `http://host[:port]`.
    #[error("invalid server url: {0}")]
    InvalidUrl(String),
    /// Plain http to a host that policy does not allow: not loopback and not
    /// marked trusted, or a public IP address (never allowed over plain http).
    #[error(
        "refusing plain http to `{0}`: plain http is for loopback, or for a private-network \
         address or host when trusted_private_network is set (never for a public IP); \
         otherwise use https"
    )]
    InsecureTransport(String),
    /// A setting the client cannot honor (a zero timeout). Not transient.
    #[error("invalid client setting: {0}")]
    InvalidConfig(String),
    /// `https://` needs a TLS transport, which this crate does not have yet.
    #[error("https is not supported yet: no TLS transport is built into tole-client")]
    HttpsUnsupported,
    /// The server answered a non-2xx status. `message` is the server's
    /// `{"error": ...}` text when it sent one.
    #[error("server answered {status}: {message}")]
    Status {
        status: u16,
        message: String,
        body: Value,
    },
    /// The peer did not speak the expected HTTP / SSE.
    #[error("malformed response: {0}")]
    Protocol(String),
    /// The body was JSON of an unexpected shape.
    #[error("could not decode the server's JSON: {0}")]
    Decode(#[from] serde_json::Error),
    /// The event stream could not be re-established.
    #[error("event stream lost: gave up after {0} failed reconnect attempts")]
    ReconnectExhausted(u32),
}

impl From<std::io::Error> for Error {
    /// The conversion every `?` on an i/o call goes through. An `InvalidData`
    /// error is what the HTTP and SSE readers raise for a malformed peer
    /// (a bad chunk size, a chunk without its CRLF, too many trailers, a line
    /// that is too long): a [`Error::Protocol`], not a dropped connection. An
    /// `InvalidInput` is a setting the socket refused (a zero timeout): an
    /// [`Error::InvalidConfig`]. Neither is ever retried.
    fn from(e: std::io::Error) -> Self {
        match e.kind() {
            std::io::ErrorKind::InvalidData => Error::Protocol(e.to_string()),
            std::io::ErrorKind::InvalidInput => Error::InvalidConfig(e.to_string()),
            _ => Error::Io(e),
        }
    }
}

impl Error {
    /// The failure says nothing about the request itself, so trying an
    /// IDEMPOTENT request again (a GET) on a fresh connection may succeed:
    /// the connection could not be opened, the peer reset or closed it, or the
    /// server said to come back later (429, 502, 503, 504).
    ///
    /// Not transient: a timeout (a response that exceeded its deadline after
    /// the request was sent, or an idle stream), a malformed answer, a bad
    /// setting, a policy refusal, and any other status. Never use this to
    /// replay a POST (a prompt, an approval decision, a cancel): once its bytes
    /// were sent the server may have acted. This client never retries a call
    /// on its own; only the event stream, a GET, reconnects.
    pub fn is_transient(&self) -> bool {
        use std::io::ErrorKind::*;
        match self {
            Error::Connect(_) => true,
            Error::Io(e) => matches!(
                e.kind(),
                ConnectionReset | ConnectionAborted | BrokenPipe | UnexpectedEof | NotConnected
            ),
            Error::Status { status, .. } => matches!(status, 429 | 502 | 503 | 504),
            _ => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Error as IoError, ErrorKind};

    #[test]
    fn io_errors_are_classified_by_what_they_say_about_the_request() {
        let io = |k| Error::from(IoError::new(k, "x"));
        // A malformed peer and a bad setting are their own errors.
        assert!(matches!(io(ErrorKind::InvalidData), Error::Protocol(_)));
        assert!(matches!(
            io(ErrorKind::InvalidInput),
            Error::InvalidConfig(_)
        ));
        for dropped in [
            ErrorKind::ConnectionReset,
            ErrorKind::ConnectionAborted,
            ErrorKind::BrokenPipe,
            ErrorKind::UnexpectedEof,
            ErrorKind::NotConnected,
        ] {
            assert!(io(dropped).is_transient(), "{dropped:?}");
        }
        // A deadline that passed after the request was sent is not retried,
        // nor is anything the client itself raised.
        for not in [ErrorKind::TimedOut, ErrorKind::WouldBlock, ErrorKind::Other] {
            assert!(!io(not).is_transient(), "{not:?}");
        }
        assert!(Error::Connect(IoError::from(ErrorKind::TimedOut)).is_transient());
        assert!(!Error::Protocol("x".into()).is_transient());
        assert!(!Error::InvalidConfig("x".into()).is_transient());
        assert!(!Error::InsecureTransport("x".into()).is_transient());
        let status = |status| Error::Status {
            status,
            message: String::new(),
            body: Value::Null,
        };
        for retry in [429, 502, 503, 504] {
            assert!(status(retry).is_transient(), "{retry}");
        }
        for verdict in [400, 401, 404, 409, 500] {
            assert!(!status(verdict).is_transient(), "{verdict}");
        }
    }
}
