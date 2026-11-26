//! HTTPS to a remote `tole serve` (#1071): the TLS transport.
//!
//! `tole serve` does not speak TLS itself; a remote one sits behind a reverse
//! proxy that does (Caddy, nginx, a cloud load balancer: `docs/mobile-control.md`).
//! This transport is the client's side of that: it opens a TCP connection,
//! completes a TLS handshake, and only then hands the stream to the HTTP layer.
//! The proxy forwards plain HTTP to serve, so serve needs no change.
//!
//! What it guarantees:
//!
//! - **The certificate is always verified**: the chain, the validity dates and
//!   that it is for the host in the URL. There is no flag, builder method or
//!   environment variable that turns this off, and no "accept any
//!   certificate". A server whose certificate does not verify is refused with
//!   [`Error::Tls`] before a single byte of the request, the bearer token
//!   included, has been written.
//! - **Which authorities are trusted** is explicit: by default the Mozilla
//!   root list bundled in the build (`webpki-roots`, so the answer is the same
//!   on every platform and does not depend on a system store). A self-hosted
//!   server with a private CA or a self-signed certificate is reached by
//!   trusting exactly that certificate ([`TlsTransport::with_ca_pem`]), which
//!   replaces the list rather than adding to it.
//! - **TLS 1.2 or 1.3**, via `rustls` with the `ring` provider (the one the
//!   rest of tole already uses; no OpenSSL, no aws-lc).
//!
//! Unlike plain `http://` ([`crate::TcpTransport`]) there is no address policy:
//! the certificate, not the network, is what authenticates the server, so any
//! host is allowed and a name's resolved addresses are not filtered.

use crate::client::Client;
use crate::error::{Error, Result};
use crate::transport::{connect_budget, dial, parse_authority, Authority, Connection, Transport};
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, ServerName};
use rustls::{ClientConfig, ClientConnection, RootCertStore, StreamOwned};
use std::fmt;
use std::io::{self, Read, Write};
use std::net::{SocketAddr, TcpStream, ToSocketAddrs};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// HTTPS over TCP with a verified server certificate.
#[derive(Clone)]
pub struct TlsTransport {
    host: String,
    port: u16,
    server_name: ServerName<'static>,
    config: Arc<ClientConfig>,
    connect_timeout: Duration,
}

impl fmt::Debug for TlsTransport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TlsTransport")
            .field("host", &self.host)
            .field("port", &self.port)
            .field("connect_timeout", &self.connect_timeout)
            .finish_non_exhaustive()
    }
}

/// A client config that trusts exactly `roots`.
fn config_for(roots: RootCertStore) -> Result<Arc<ClientConfig>> {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let mut config = ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .map_err(|e| Error::InvalidConfig(format!("no usable TLS protocol version: {e}")))?
        .with_root_certificates(roots)
        .with_no_client_auth();
    // serve speaks HTTP/1.1; say so, so a proxy that offers h2 does not pick it.
    config.alpn_protocols = vec![b"http/1.1".to_vec()];
    Ok(Arc::new(config))
}

impl TlsTransport {
    /// `url` is `https://host[:port]` (default port 443; no userinfo, path,
    /// query or fragment: the serve face is root-mounted). The host is a DNS
    /// name or an IP literal under the same rules as [`crate::TcpTransport`];
    /// the certificate must be valid for it (an IP literal needs an IP
    /// subject-alternative-name). Trusts the bundled Mozilla roots.
    ///
    /// `http://` is refused here ([`Error::InvalidUrl`]); it is
    /// [`crate::TcpTransport`]'s, with its own policy.
    pub fn new(url: &str) -> Result<Self> {
        let Authority { host, port, ip } = parse_authority(url, "https", 443)?;
        let server_name = match ip {
            Some(ip) => ServerName::IpAddress(ip.into()),
            None => ServerName::try_from(host.clone())
                .map_err(|_| Error::InvalidUrl("the host is not a valid TLS server name".into()))?,
        };
        let roots = RootCertStore::from_iter(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
        Ok(Self {
            host,
            port,
            server_name,
            config: config_for(roots)?,
            connect_timeout: Duration::from_secs(10),
        })
    }

    /// Trust exactly the certificates in `pem` (one or more `CERTIFICATE`
    /// blocks) INSTEAD of the bundled roots: the way to reach a server with a
    /// private CA or a self-signed certificate. Pass the CA that signed the
    /// server's certificate, or the self-signed certificate itself. Because it
    /// replaces the bundled list, a server whose chain ends anywhere else is
    /// refused, which pins the connection to that authority.
    ///
    /// The host name and dates are still checked. No certificate, or one that
    /// does not parse, is [`Error::InvalidConfig`].
    pub fn with_ca_pem(mut self, pem: &[u8]) -> Result<Self> {
        let mut roots = RootCertStore::empty();
        for cert in CertificateDer::pem_slice_iter(pem) {
            let cert = cert
                .map_err(|e| Error::InvalidConfig(format!("the CA certificate is not PEM: {e}")))?;
            roots.add(cert).map_err(|e| {
                Error::InvalidConfig(format!("the CA certificate is not usable: {e}"))
            })?;
        }
        if roots.is_empty() {
            return Err(Error::InvalidConfig(
                "no CERTIFICATE block in the CA PEM".into(),
            ));
        }
        self.config = config_for(roots)?;
        Ok(self)
    }

    /// Bound the whole connection set-up, TCP connect and TLS handshake
    /// together (default 10 s, the same as [`crate::TcpTransport`]). Zero is
    /// refused ([`Error::InvalidConfig`]).
    pub fn with_connect_timeout(mut self, timeout: Duration) -> Result<Self> {
        if timeout.is_zero() {
            return Err(Error::InvalidConfig(
                "the connect timeout cannot be zero".into(),
            ));
        }
        self.connect_timeout = timeout;
        Ok(self)
    }

    /// The TLS handshake on `sock`, every step bounded by what is left of the
    /// budget that started at `started` (a server that sends a byte at a time
    /// cannot stretch it).
    fn handshake(&self, mut sock: TcpStream, started: Instant) -> Result<TlsConnection> {
        let mut conn = ClientConnection::new(Arc::clone(&self.config), self.server_name.clone())
            .map_err(|e| self.refused(&e))?;
        let timed_out = || {
            Error::Connect(io::Error::new(
                io::ErrorKind::TimedOut,
                "the TLS handshake did not finish in time",
            ))
        };
        let io_err = |e: io::Error| match e.kind() {
            io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut => timed_out(),
            _ => Error::Connect(e),
        };
        loop {
            let Some(left) = connect_budget(started, self.connect_timeout) else {
                return Err(timed_out());
            };
            sock.set_read_timeout(Some(left)).map_err(io_err)?;
            sock.set_write_timeout(Some(left)).map_err(io_err)?;
            while conn.wants_write() {
                conn.write_tls(&mut sock).map_err(io_err)?;
            }
            if !conn.is_handshaking() {
                break;
            }
            if conn.wants_read() {
                if conn.read_tls(&mut sock).map_err(io_err)? == 0 {
                    return Err(Error::Connect(io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        "the server closed the connection during the TLS handshake",
                    )));
                }
                if let Err(e) = conn.process_new_packets() {
                    // Best effort: tell the peer why (the alert is queued).
                    let _ = conn.write_tls(&mut sock);
                    return Err(self.refused(&e));
                }
            }
        }
        // The caller sets the per-read bounds of the request itself.
        sock.set_read_timeout(None).map_err(io_err)?;
        sock.set_write_timeout(None).map_err(io_err)?;
        Ok(TlsConnection {
            stream: StreamOwned::new(conn, Bounded::new(sock)),
        })
    }

    fn refused(&self, e: &rustls::Error) -> Error {
        Error::Tls(format!(
            "the TLS handshake with `{}` failed: {e}",
            self.host
        ))
    }
}

impl Transport for TlsTransport {
    /// Resolves the name, connects, and completes the handshake within ONE
    /// budget ([`TlsTransport::with_connect_timeout`]); name resolution
    /// itself is a blocking system call std cannot bound. A server whose
    /// certificate does not verify is [`Error::Tls`] (never retried); the
    /// rest are [`Error::Connect`]. Nothing of the request has been sent when
    /// this returns an error.
    fn connect(&self) -> Result<Box<dyn Connection>> {
        let started = Instant::now();
        let addrs: Vec<SocketAddr> = (self.host.as_str(), self.port)
            .to_socket_addrs()
            .map_err(Error::Connect)?
            .collect();
        let sock = dial(&addrs, started, self.connect_timeout)?;
        Ok(Box::new(self.handshake(sock, started)?))
    }

    fn host_header(&self) -> String {
        let host = if self.host.contains(':') {
            format!("[{}]", self.host)
        } else {
            self.host.clone()
        };
        if self.port == 443 {
            host
        } else {
            format!("{host}:{}", self.port)
        }
    }
}

/// Does `url` start with the `https://` scheme, in any letter case (RFC 3986)?
/// Anything else, a malformed scheme included, is left to the plain transport,
/// which refuses what it does not accept: the choice fails closed.
pub(crate) fn is_https(url: &str) -> bool {
    url.get(..8)
        .is_some_and(|p| p.eq_ignore_ascii_case("https://"))
}

/// The entry points that pick HTTPS. They live here, not in `client.rs`, whose
/// size is capped (`scripts/arch-size-allow.txt`).
impl Client {
    /// The transport the scheme of `url` names: `http://host[:port]` is plain
    /// HTTP ([`Client::connect_tcp`], loopback or a trusted private network),
    /// `https://host[:port]` is HTTPS with a verified certificate
    /// ([`Client::connect_https`]). `trusted_private_network` only widens
    /// plain `http://`; it has no effect on `https://`, which is allowed to
    /// any host because the certificate authenticates it.
    pub fn connect(
        url: &str,
        token: Option<String>,
        trusted_private_network: bool,
    ) -> Result<Self> {
        if is_https(url) {
            Self::connect_https(url, token)
        } else {
            Self::connect_tcp(url, token, trusted_private_network)
        }
    }

    /// HTTPS to `url` (`https://host[:port]`), the remote case: a `tole serve`
    /// behind a TLS reverse proxy. The server certificate must verify against
    /// the bundled Mozilla roots and be valid for the host, or every call
    /// fails with [`Error::Tls`] before the token is sent. For a private CA or
    /// a self-signed certificate build the transport yourself
    /// ([`TlsTransport::with_ca_pem`]) and pass it to [`Client::new`].
    pub fn connect_https(url: &str, token: Option<String>) -> Result<Self> {
        let client = Self::new(TlsTransport::new(url)?);
        Ok(match token {
            Some(t) => client.with_token(t),
            None => client,
        })
    }
}

/// The TCP socket under the TLS session, with deadlines instead of timeouts.
///
/// A socket timeout bounds ONE socket read. A single `read` of the TLS stream
/// can make many (rustls loops over the socket until it holds one whole
/// record, up to 16 KiB), so a peer that sends a byte just inside the timeout
/// could hold one `read` for the timeout times the record size. Here a timeout
/// set by the caller becomes an absolute point in time, and EVERY socket read
/// or write below it is given what is left, failing with `TimedOut` once it is
/// gone: the bound holds for the call, not for each of its pieces.
struct Bounded {
    sock: TcpStream,
    read_until: Option<Instant>,
    write_until: Option<Instant>,
}

/// What is left until `at`, for a socket timeout; `TimedOut` once it is spent.
fn left_until(at: Instant) -> io::Result<Duration> {
    let left = at.saturating_duration_since(Instant::now());
    if left.is_zero() {
        return Err(io::Error::new(
            io::ErrorKind::TimedOut,
            "the deadline has passed",
        ));
    }
    Ok(left)
}

impl Bounded {
    fn new(sock: TcpStream) -> Self {
        Self {
            sock,
            read_until: None,
            write_until: None,
        }
    }
}

impl Read for Bounded {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if let Some(at) = self.read_until {
            self.sock.set_read_timeout(Some(left_until(at)?))?;
        }
        self.sock.read(buf)
    }
}

impl Write for Bounded {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if let Some(at) = self.write_until {
            self.sock.set_write_timeout(Some(left_until(at)?))?;
        }
        self.sock.write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.sock.flush()
    }
}

/// `now + timeout`; `None` for no bound (also when the sum overflows).
fn deadline(timeout: Option<Duration>) -> Option<Instant> {
    timeout.and_then(|t| Instant::now().checked_add(t))
}

/// An established TLS session over a TCP socket.
struct TlsConnection {
    stream: StreamOwned<ClientConnection, Bounded>,
}

impl Read for TlsConnection {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.stream.read(buf)
    }
}

impl Write for TlsConnection {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.stream.write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.stream.flush()
    }
}

impl Connection for TlsConnection {
    /// A zero timeout is invalid, as for a socket. The bound is a deadline
    /// that every socket read of the next `read` call shares (see [`Bounded`]).
    fn set_read_timeout(&mut self, timeout: Option<Duration>) -> io::Result<()> {
        let sock = &mut self.stream.sock;
        sock.sock.set_read_timeout(timeout)?; // validates; `None` clears it
        sock.read_until = deadline(timeout);
        Ok(())
    }

    fn set_write_timeout(&mut self, timeout: Option<Duration>) -> io::Result<()> {
        let sock = &mut self.stream.sock;
        sock.sock.set_write_timeout(timeout)?;
        sock.write_until = deadline(timeout);
        Ok(())
    }
}

impl Drop for TlsConnection {
    /// Say goodbye (`close_notify`), best effort and briefly bounded.
    fn drop(&mut self) {
        self.stream.conn.send_close_notify();
        self.stream.sock.write_until = deadline(Some(Duration::from_millis(500)));
        let _ = self.stream.conn.write_tls(&mut self.stream.sock);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn urls_follow_the_tcp_transports_rules_with_443_as_default() {
        let t = TlsTransport::new("https://example.com").unwrap();
        assert_eq!((t.host.as_str(), t.port), ("example.com", 443));
        assert_eq!(t.host_header(), "example.com");
        let t = TlsTransport::new("https://example.com:8443/").unwrap();
        assert_eq!(t.host_header(), "example.com:8443");
        let t = TlsTransport::new("https://[2001:db8::1]").unwrap();
        assert_eq!(t.host_header(), "[2001:db8::1]");
        let t = TlsTransport::new("https://203.0.113.9:7777").unwrap();
        assert_eq!(t.host_header(), "203.0.113.9:7777");
        // Any host is fine: the certificate authenticates it, not the network.
        for ok in [
            "https://8.8.8.8",
            "https://box1.lan:7777",
            "https://localhost",
        ] {
            assert!(TlsTransport::new(ok).is_ok(), "{ok}");
        }
    }

    #[test]
    fn the_scheme_is_case_insensitive() {
        assert!(is_https("HTTPS://example.com") && is_https("hTtPs://x") && is_https("https://"));
        assert!(!is_https("http://example.com") && !is_https("https:/") && !is_https("é"));
        let t = TlsTransport::new("HTTPS://example.com:8443").unwrap();
        assert_eq!(t.host_header(), "example.com:8443");
        // The plain transport refuses it in any case, and so does the other way.
        assert!(matches!(
            crate::TcpTransport::new("HtTpS://127.0.0.1", true),
            Err(Error::InvalidUrl(_))
        ));
        assert!(crate::TcpTransport::new("HTTP://127.0.0.1:9", false).is_ok());
        assert!(matches!(
            TlsTransport::new("HTTP://127.0.0.1:9"),
            Err(Error::InvalidUrl(_))
        ));
    }

    #[test]
    fn malformed_and_plain_urls_are_refused() {
        for bad in [
            "http://127.0.0.1:7777",
            "example.com",
            "https://",
            "https://u:p@example.com",
            "https://example.com/x",
            "https://example.com?a=1",
            "https://example.com:0",
            "https://example.com:x",
            "https://134744072",
            "https://0x08080808",
            "https://8.8.8",
            "https://[fe80::1%eth0]",
            "https://exa mple.com",
        ] {
            assert!(
                matches!(TlsTransport::new(bad), Err(Error::InvalidUrl(_))),
                "{bad}"
            );
        }
    }

    #[test]
    fn a_bad_ca_or_timeout_is_a_config_error() {
        let t = TlsTransport::new("https://example.com").unwrap();
        for pem in [
            &b""[..],
            b"not pem at all",
            b"-----BEGIN CERTIFICATE-----\n!!\n-----END CERTIFICATE-----\n",
        ] {
            assert!(
                matches!(t.clone().with_ca_pem(pem), Err(Error::InvalidConfig(_))),
                "{:?}",
                String::from_utf8_lossy(pem)
            );
        }
        assert!(matches!(
            t.with_connect_timeout(Duration::ZERO),
            Err(Error::InvalidConfig(_))
        ));
    }

    #[test]
    fn debug_shows_the_address_only() {
        let t = TlsTransport::new("https://example.com:8443").unwrap();
        let text = format!("{t:?}");
        assert!(
            text.contains("example.com") && text.contains("8443"),
            "{text}"
        );
    }
}
