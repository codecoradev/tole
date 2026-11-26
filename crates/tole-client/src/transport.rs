//! The byte connection under the client.
//!
//! A [`Transport`] opens one fresh [`Connection`] per request (`tole serve`
//! answers one request per connection and closes it). Everything above it,
//! HTTP framing included, is transport-independent.
//!
//! # Transports
//!
//! [`TcpTransport`] is plain HTTP over TCP (loopback, or a trusted private
//! network). [`TlsTransport`](crate::TlsTransport) is HTTPS over TCP, for a
//! remote serve behind a TLS reverse proxy (#1071). On Unix, `UnixTransport`
//! (in the crate root) is HTTP over the local socket of `tole serve --socket`
//! (#1067). Each is a small type behind this trait and the client does not
//! know which it has. A Windows named pipe (#1051) is the same shape.

use crate::error::{Error, Result};
use std::io::{self, Read, Write};
use std::net::{IpAddr, SocketAddr, TcpStream, ToSocketAddrs};
use std::time::{Duration, Instant};

/// An open, bidirectional byte stream to one serve.
pub trait Connection: Read + Write + Send {
    /// Bound a single blocking read (`None` = wait forever). A zero timeout
    /// is invalid.
    fn set_read_timeout(&mut self, timeout: Option<Duration>) -> io::Result<()>;
    /// Bound a single blocking write (`None` = wait forever). A zero timeout
    /// is invalid.
    fn set_write_timeout(&mut self, timeout: Option<Duration>) -> io::Result<()>;
}

impl Connection for TcpStream {
    fn set_read_timeout(&mut self, timeout: Option<Duration>) -> io::Result<()> {
        TcpStream::set_read_timeout(self, timeout)
    }

    fn set_write_timeout(&mut self, timeout: Option<Duration>) -> io::Result<()> {
        TcpStream::set_write_timeout(self, timeout)
    }
}

/// How to reach a serve.
pub trait Transport: Send + Sync {
    /// Open a new connection. A policy refusal is an [`Error`] that is not
    /// transient ([`Error::InsecureTransport`]), so it is never retried.
    fn connect(&self) -> Result<Box<dyn Connection>>;
    /// The `Host` header value.
    fn host_header(&self) -> String;
}

/// Plain HTTP over TCP. Loopback only unless `trusted_private_network` is set.
#[derive(Debug, Clone)]
pub struct TcpTransport {
    host: String,
    port: u16,
    trusted: bool,
    connect_timeout: Duration,
}

impl TcpTransport {
    /// `url` is `http://host[:port]` (no userinfo, path, query or fragment: the
    /// serve face is root-mounted). `https://` is refused here
    /// ([`Error::InvalidUrl`]): it is [`TlsTransport`](crate::TlsTransport)'s.
    ///
    /// The host must be an IP literal (IPv6 in brackets, no scope id) or a DNS
    /// name; shorthand numeric forms (`134744072`, `0x08080808`, `8.8.8`),
    /// which the system resolver would turn into an IP, are refused.
    ///
    /// A host that is not loopback (`localhost`, `127.0.0.0/8`, `::1`) is
    /// refused unless `trusted_private_network` says the owner marked that
    /// network as trusted. The flag only widens loopback to private ranges
    /// (RFC 1918, link-local, CGNAT, IPv6 ULA and link-local), never to a
    /// public address: an IP literal outside them is refused, and at connect
    /// time ([`Transport::connect`]) every address a name resolves to is
    /// checked the same way, so a trusted hostname that resolves to a public
    /// address is refused too. Without the flag only loopback addresses are
    /// connected to (`localhost` included).
    pub fn new(url: &str, trusted_private_network: bool) -> Result<Self> {
        if url.starts_with("https://") {
            return Err(Error::InvalidUrl(
                "https:// is not plain http: use TlsTransport, or Client::connect, which picks \
                 the transport from the scheme"
                    .into(),
            ));
        }
        let Authority { host, port, ip } = parse_authority(url, "http", 80)?;
        if !is_loopback(&host) {
            let literal_ok = ip.is_none_or(is_private_or_local);
            if !trusted_private_network || !literal_ok {
                return Err(Error::InsecureTransport(host));
            }
        }
        Ok(Self {
            host,
            port,
            trusted: trusted_private_network,
            connect_timeout: Duration::from_secs(10),
        })
    }
}

/// The host and port of a validated server url.
pub(crate) struct Authority {
    pub(crate) host: String,
    pub(crate) port: u16,
    /// The host when it is an IP literal.
    pub(crate) ip: Option<IpAddr>,
}

/// Parse `<scheme>://host[:port]` (no userinfo, path, query or fragment: the
/// serve face is root-mounted; a trailing `/` is allowed). The host must be an
/// IP literal (IPv6 in brackets, no scope id) or a DNS name; shorthand numeric
/// forms (`134744072`, `0x08080808`, `8.8.8`), which the system resolver would
/// turn into an IP, are refused. Shared by the plain and the TLS transport so
/// both accept exactly the same addresses.
pub(crate) fn parse_authority(url: &str, scheme: &str, default_port: u16) -> Result<Authority> {
    let bad = |why: &str| Error::InvalidUrl(why.to_string());
    let prefix = format!("{scheme}://");
    let Some(rest) = url.strip_prefix(prefix.as_str()) else {
        return Err(Error::InvalidUrl(format!(
            "expected {scheme}://host[:port]"
        )));
    };
    let authority = rest.strip_suffix('/').unwrap_or(rest);
    if authority.is_empty() || authority.contains(['/', '?', '#', '@', ' ']) {
        return Err(Error::InvalidUrl(format!(
            "expected only host[:port] after {scheme}://"
        )));
    }
    let (host, port) = match authority.strip_prefix('[') {
        Some(v6) => {
            let (h, tail) = v6
                .split_once(']')
                .ok_or_else(|| bad("unclosed [ in host"))?;
            match tail {
                "" => (h, default_port),
                t => (
                    h,
                    parse_port(t.strip_prefix(':').ok_or_else(|| bad("bad port"))?)?,
                ),
            }
        }
        None => match authority.rsplit_once(':') {
            Some((h, p)) => (h, parse_port(p)?),
            None => (authority, default_port),
        },
    };
    if host.is_empty() {
        return Err(bad("empty host"));
    }
    // Only forms the connect-time policy can reason about: a strict IP
    // literal, or a DNS name. Anything else (a scope id, `0x08080808`,
    // `8.8.8`) is left to the system resolver's looser syntax, which can
    // turn it into a public IP.
    let ip = host.parse::<IpAddr>().ok();
    let bracketed = authority.starts_with('[');
    let host_ok = match ip {
        Some(ip) => ip.is_ipv6() == bracketed,
        None => !bracketed && is_dns_name(host),
    };
    if !host_ok {
        return Err(bad("the host must be an IP address or a DNS name"));
    }
    Ok(Authority {
        host: host.to_string(),
        port,
        ip,
    })
}

/// 1 to 65535, ASCII digits only (no sign, no port 0: it cannot be connected to).
fn parse_port(p: &str) -> Result<u16> {
    let ok = !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit());
    match p.parse::<u16>() {
        Ok(n) if ok && n > 0 => Ok(n),
        _ => Err(Error::InvalidUrl("the port must be 1 to 65535".into())),
    }
}

/// A DNS name the resolver cannot mistake for an IP: dot-separated labels of
/// letters, digits, `-` and `_`, and a last label that is not numeric (all
/// digits, or `0x` and hex digits), the shapes `inet_aton` reads as addresses.
fn is_dns_name(host: &str) -> bool {
    let labels: Vec<&str> = host.split('.').collect();
    let label_ok = |l: &&str| {
        (1..=63).contains(&l.len())
            && l.bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
            && !l.starts_with('-')
            && !l.ends_with('-')
    };
    let last = labels[labels.len() - 1];
    let numeric = last.bytes().all(|b| b.is_ascii_digit())
        || last
            .strip_prefix("0x")
            .or_else(|| last.strip_prefix("0X"))
            .is_some_and(|h| h.bytes().all(|b| b.is_ascii_hexdigit()));
    host.len() <= 253 && labels.iter().all(label_ok) && !numeric
}

/// `localhost` or a loopback IP literal. Any other name is not trusted to
/// resolve to loopback.
fn is_loopback(host: &str) -> bool {
    host.eq_ignore_ascii_case("localhost")
        || host.parse::<IpAddr>().is_ok_and(|ip| ip.is_loopback())
}

/// RFC 1918, link-local, CGNAT (100.64.0.0/10) and loopback IPv4; loopback,
/// unique-local (fc00::/7) and link-local (fe80::/10) IPv6.
fn is_private_or_local(ip: IpAddr) -> bool {
    match ip.to_canonical() {
        IpAddr::V4(v4) => {
            let o = v4.octets();
            v4.is_private()
                || v4.is_loopback()
                || v4.is_link_local()
                || (o[0] == 100 && o[1] & 0xC0 == 64)
        }
        IpAddr::V6(v6) => {
            let first = v6.segments()[0];
            v6.is_loopback() || first & 0xFE00 == 0xFC00 || first & 0xFFC0 == 0xFE80
        }
    }
}

/// The resolved addresses a connection may use. A name is resolved by the
/// system and whatever it returns is not trusted: without `trusted` only
/// loopback addresses pass, with it loopback and the private ranges. A public
/// address never passes.
fn allowed_addrs(resolved: Vec<SocketAddr>, trusted: bool, host: &str) -> Result<Vec<SocketAddr>> {
    let usable: Vec<SocketAddr> = resolved
        .into_iter()
        .filter(|a| {
            if trusted {
                is_private_or_local(a.ip())
            } else {
                a.ip().to_canonical().is_loopback()
            }
        })
        .collect();
    if usable.is_empty() {
        return Err(Error::InsecureTransport(host.to_string()));
    }
    Ok(usable)
}

/// What is left of the connect budget, or `None` when it is spent.
pub(crate) fn connect_budget(started: Instant, total: Duration) -> Option<Duration> {
    let left = total.saturating_sub(started.elapsed());
    (!left.is_zero()).then_some(left)
}

/// Try `addrs` one after the other within ONE connect budget of `total`,
/// counted from `started` (not per address). Failures are [`Error::Connect`].
pub(crate) fn dial(addrs: &[SocketAddr], started: Instant, total: Duration) -> Result<TcpStream> {
    let mut last = io::Error::new(io::ErrorKind::NotFound, "no address resolved");
    for addr in addrs {
        let Some(left) = connect_budget(started, total) else {
            last = io::Error::new(io::ErrorKind::TimedOut, "the connect budget is spent");
            break;
        };
        match TcpStream::connect_timeout(addr, left) {
            Ok(s) => {
                let _ = s.set_nodelay(true);
                return Ok(s);
            }
            Err(e) => last = e,
        }
    }
    Err(Error::Connect(last))
}

impl Transport for TcpTransport {
    /// Resolves the name, then tries the allowed addresses one after the
    /// other within ONE connect budget (10 s in all, not per address). Name
    /// resolution itself is a blocking system call (`getaddrinfo`) that std
    /// cannot bound; use an IP literal where that matters. Failures here are
    /// [`Error::Connect`]: nothing was sent.
    fn connect(&self) -> Result<Box<dyn Connection>> {
        let resolved = (self.host.as_str(), self.port)
            .to_socket_addrs()
            .map_err(Error::Connect)?
            .collect();
        let started = Instant::now();
        let addrs = allowed_addrs(resolved, self.trusted, &self.host)?;
        Ok(Box::new(dial(&addrs, started, self.connect_timeout)?))
    }

    fn host_header(&self) -> String {
        if self.host.contains(':') {
            format!("[{}]:{}", self.host, self.port)
        } else {
            format!("{}:{}", self.host, self.port)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loopback_is_allowed_and_everything_else_needs_the_flag() {
        for ok in [
            "http://127.0.0.1:8080",
            "http://localhost:1/",
            "http://[::1]:9",
            "http://127.0.0.1",
        ] {
            assert!(TcpTransport::new(ok, false).is_ok(), "{ok}");
        }
        for host in [
            "http://10.0.0.5:8080",
            "http://example.com",
            "http://[fd00::1]:1",
        ] {
            assert!(
                matches!(
                    TcpTransport::new(host, false),
                    Err(Error::InsecureTransport(_))
                ),
                "{host}"
            );
            assert!(TcpTransport::new(host, true).is_ok(), "{host} trusted");
        }
    }

    #[test]
    fn a_public_ip_is_never_accepted_even_when_trusted() {
        for public in [
            "http://203.0.113.9:1",
            "http://8.8.8.8",
            "http://[2001:db8::1]:1",
            "http://100.128.0.1",
        ] {
            assert!(
                matches!(
                    TcpTransport::new(public, true),
                    Err(Error::InsecureTransport(_))
                ),
                "{public}"
            );
        }
        for private in [
            "http://192.168.1.2",
            "http://172.16.0.1",
            "http://169.254.1.1",
            "http://100.64.0.1",
            "http://[fd12::1]",
            "http://[fe80::1]",
            "http://tailnet-box:8080",
        ] {
            assert!(TcpTransport::new(private, true).is_ok(), "{private}");
        }
    }

    #[test]
    fn untrusted_connects_only_to_loopback_addresses() {
        let lo: SocketAddr = "127.0.0.1:9".parse().unwrap();
        let lo6: SocketAddr = "[::1]:9".parse().unwrap();
        let lan: SocketAddr = "192.168.1.2:9".parse().unwrap();
        assert_eq!(
            allowed_addrs(vec![lan, lo, lo6], false, "localhost").unwrap(),
            [lo, lo6]
        );
        // A name that resolved to nothing loopback is refused, not connected.
        assert!(matches!(
            allowed_addrs(vec![lan], false, "localhost"),
            Err(Error::InsecureTransport(_))
        ));
        assert_eq!(allowed_addrs(vec![lan], true, "box").unwrap(), [lan]);
        // Trusted widens to private ranges only: a name that resolves to a
        // public address is refused, and a mixed answer keeps the private part.
        let public: SocketAddr = "8.8.8.8:9".parse().unwrap();
        let public6: SocketAddr = "[2001:4860:4860::8888]:9".parse().unwrap();
        assert!(matches!(
            allowed_addrs(vec![public, public6], true, "box"),
            Err(Error::InsecureTransport(_))
        ));
        assert_eq!(
            allowed_addrs(vec![public, lan], true, "box").unwrap(),
            [lan]
        );
    }

    #[test]
    fn numeric_shorthand_and_scoped_hosts_are_refused_even_when_trusted() {
        // The system resolver turns each of these into a public address.
        for sneaky in [
            "http://134744072",
            "http://0x08080808",
            "http://8.8.8",
            "http://8.8.8.8.8",
            "http://010.8.8.8",
            "http://[2001:4860:4860::8888%1]",
            "http://[fe80::1%eth0]",
            "http://[8.8.8.8]",
            "http://[::1",
            "http://exa mple.com",
            "http://-bad.example",
        ] {
            for trusted in [false, true] {
                assert!(
                    TcpTransport::new(sneaky, trusted).is_err(),
                    "{sneaky} trusted={trusted}"
                );
            }
        }
        assert!(matches!(
            TcpTransport::new("http://8.8.8.8", true),
            Err(Error::InsecureTransport(_))
        ));
        // Ordinary names, including digits and underscores inside labels, pass.
        for name in ["http://box1.lan:7777", "http://my_host", "http://a-b.c9"] {
            assert!(TcpTransport::new(name, true).is_ok(), "{name}");
        }
    }

    #[test]
    fn https_and_malformed_urls_are_refused() {
        // https is the TLS transport's: this one never speaks it, even to
        // loopback or when trusted.
        assert!(matches!(
            TcpTransport::new("https://127.0.0.1", true),
            Err(Error::InvalidUrl(_))
        ));
        for bad in [
            "127.0.0.1:80",
            "http://",
            "http://u:p@127.0.0.1",
            "http://127.0.0.1/x",
            "http://127.0.0.1:x",
            "http://127.0.0.1?a=1",
        ] {
            assert!(
                matches!(TcpTransport::new(bad, true), Err(Error::InvalidUrl(_))),
                "{bad}"
            );
        }
    }

    #[test]
    fn ports_are_one_to_65535_digits_only() {
        for ok in [
            "http://127.0.0.1:1",
            "http://127.0.0.1:65535",
            "http://localhost:8080",
        ] {
            assert!(TcpTransport::new(ok, false).is_ok(), "{ok}");
        }
        for bad in [
            "http://127.0.0.1:0",
            "http://127.0.0.1:+80",
            "http://127.0.0.1:-1",
            "http://127.0.0.1:65536",
            "http://127.0.0.1:",
            "http://[::1]:0",
            "http://[::1]:+9",
        ] {
            assert!(
                matches!(TcpTransport::new(bad, false), Err(Error::InvalidUrl(_))),
                "{bad}"
            );
        }
    }

    #[test]
    fn the_connect_budget_is_shared_by_all_addresses() {
        let long_ago = Instant::now() - Duration::from_secs(30);
        assert_eq!(connect_budget(long_ago, Duration::from_secs(10)), None);
        let left = connect_budget(Instant::now(), Duration::from_secs(10)).unwrap();
        assert!(left <= Duration::from_secs(10) && left > Duration::from_secs(8));
    }

    #[test]
    fn a_refused_connection_is_a_transient_connect_error() {
        // Bind to learn a free port, then close it: nothing listens there.
        let port = std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let t = TcpTransport::new(&format!("http://127.0.0.1:{port}"), false).unwrap();
        let err = t.connect().err().expect("nothing listens");
        assert!(matches!(err, Error::Connect(_)), "{err:?}");
        assert!(err.is_transient());
    }

    #[test]
    fn host_header_brackets_ipv6() {
        let t = TcpTransport::new("http://[::1]:9", false).unwrap();
        assert_eq!(t.host_header(), "[::1]:9");
        let t = TcpTransport::new("http://localhost:7", false).unwrap();
        assert_eq!(t.host_header(), "localhost:7");
    }
}
