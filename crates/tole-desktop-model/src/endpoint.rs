//! The address of a remote server and the rule that decides whether the
//! desktop may talk to it (owner decision D6, `docs/desktop-design.md`
//! section 0): `https://` for any server, plain `http://` only for loopback or
//! for a server the owner marked as a trusted private network (a VPN or a
//! network they control), and never for a public IP address.
//!
//! The rule mirrors `tole-client`'s `TcpTransport::new`, so an entry accepted
//! here is one the client accepts. The model cannot depend on `tole-client`
//! (it has no tole dependency), so the check is repeated here and tested with
//! the same table. A name that is trusted is still checked by the client at
//! connect time against every address it resolves to.
//!
//! The address carries no credential: a `user:password@` part is refused, and
//! the type has no field a secret could be put in.

use std::net::IpAddr;

/// `https` or `http`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scheme {
    Https,
    Http,
}

/// Why plain `http://` was refused for a host.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlainHttp {
    /// A host that is not loopback, and the owner did not mark the server as a
    /// trusted private network.
    NotTrusted,
    /// A public IP address: refused even when the server is marked trusted.
    PublicAddress,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EndpointError {
    Empty,
    /// Not `http://` or `https://`.
    Scheme,
    /// A `user:password@` part: a credential never goes in the address.
    CredentialInUrl,
    /// A query or fragment: a server address has neither.
    QueryOrFragment,
    /// No host, a scope id, a shorthand number such as `0x08080808`, or a name
    /// that is not a DNS name.
    Host,
    /// Not 1 to 65535.
    Port,
    /// A path with a space or a control character.
    Path,
    /// Plain `http://` to `host`, and why it is refused.
    PlainHttp {
        host: String,
        why: PlainHttp,
    },
}

/// A validated server address.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Endpoint {
    scheme: Scheme,
    host: String,
    port: Option<u16>,
    /// A path prefix (a reverse proxy mount), without a trailing slash; empty
    /// for none.
    path: String,
}

impl Endpoint {
    /// Parse `url` under the rule above. `trusted_private_network` is the
    /// owner's mark on this server.
    pub fn parse(url: &str, trusted_private_network: bool) -> Result<Endpoint, EndpointError> {
        let url = url.trim();
        if url.is_empty() {
            return Err(EndpointError::Empty);
        }
        let lower = url.get(..8).unwrap_or(url).to_ascii_lowercase();
        let (scheme, rest) = if lower.starts_with("https://") {
            (Scheme::Https, &url[8..])
        } else if lower.starts_with("http://") {
            (Scheme::Http, &url[7..])
        } else {
            return Err(EndpointError::Scheme);
        };
        if rest.contains(['?', '#']) {
            return Err(EndpointError::QueryOrFragment);
        }
        let (authority, path) = match rest.find('/') {
            Some(i) => (&rest[..i], &rest[i..]),
            None => (rest, ""),
        };
        if authority.contains('@') {
            return Err(EndpointError::CredentialInUrl);
        }
        if path.chars().any(|c| c.is_whitespace() || c.is_control()) {
            return Err(EndpointError::Path);
        }
        let (host, port) = split_host_port(authority)?;
        let endpoint = Endpoint {
            scheme,
            host: host.to_ascii_lowercase(),
            port,
            path: path.trim_end_matches('/').to_owned(),
        };
        if scheme == Scheme::Http {
            endpoint.check_plain_http(trusted_private_network)?;
        }
        Ok(endpoint)
    }

    fn check_plain_http(&self, trusted: bool) -> Result<(), EndpointError> {
        if is_loopback(&self.host) {
            return Ok(());
        }
        let refuse = |why| EndpointError::PlainHttp {
            host: self.host.clone(),
            why,
        };
        if !trusted {
            return Err(refuse(PlainHttp::NotTrusted));
        }
        match self.host.parse::<IpAddr>() {
            Ok(ip) if !is_private_or_local(ip) => Err(refuse(PlainHttp::PublicAddress)),
            _ => Ok(()),
        }
    }

    pub fn scheme(&self) -> Scheme {
        self.scheme
    }

    pub fn host(&self) -> &str {
        &self.host
    }

    /// Whether the host is `localhost` or a loopback address.
    pub fn is_loopback(&self) -> bool {
        is_loopback(&self.host)
    }

    /// The normalised address (lower-case host, no trailing slash), for display
    /// and for the client.
    pub fn to_url(&self) -> String {
        let scheme = match self.scheme {
            Scheme::Https => "https",
            Scheme::Http => "http",
        };
        let host = if self.host.contains(':') {
            format!("[{}]", self.host)
        } else {
            self.host.clone()
        };
        match self.port {
            Some(p) => format!("{scheme}://{host}:{p}{}", self.path),
            None => format!("{scheme}://{host}{}", self.path),
        }
    }
}

fn split_host_port(authority: &str) -> Result<(&str, Option<u16>), EndpointError> {
    let (host, port, bracketed) = match authority.strip_prefix('[') {
        Some(v6) => {
            let (h, tail) = v6.split_once(']').ok_or(EndpointError::Host)?;
            let port = match tail {
                "" => None,
                t => Some(t.strip_prefix(':').ok_or(EndpointError::Port)?),
            };
            (h, port, true)
        }
        None => match authority.rsplit_once(':') {
            Some((h, p)) => (h, Some(p), false),
            None => (authority, None, false),
        },
    };
    let port = port.map(parse_port).transpose()?;
    let host_ok = match host.parse::<IpAddr>() {
        Ok(ip) => ip.is_ipv6() == bracketed,
        Err(_) => !bracketed && is_dns_name(host),
    };
    if host.is_empty() || !host_ok {
        return Err(EndpointError::Host);
    }
    Ok((host, port))
}

/// 1 to 65535, ASCII digits only.
fn parse_port(p: &str) -> Result<u16, EndpointError> {
    match p.parse::<u16>() {
        Ok(n) if n > 0 && p.bytes().all(|b| b.is_ascii_digit()) => Ok(n),
        _ => Err(EndpointError::Port),
    }
}

/// A DNS name a resolver cannot mistake for an IP: dot-separated labels of
/// letters, digits, `-` and `_`, and a last label that is not numeric (all
/// digits, or `0x` and hex digits), which `inet_aton` reads as an address.
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

/// `localhost` or a loopback IP literal. Any other name is not taken to
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

#[cfg(test)]
mod tests {
    use super::*;

    fn plain(url: &str, trusted: bool) -> Result<Endpoint, EndpointError> {
        Endpoint::parse(url, trusted)
    }

    #[test]
    fn https_is_accepted_for_any_host_trusted_or_not() {
        for url in [
            "https://tole.example.com",
            "https://tole.example.com:8443/tole/",
            "https://203.0.113.9",
            "https://[2001:db8::1]:1",
            "HTTPS://Tole.Example.COM",
        ] {
            assert!(plain(url, false).is_ok(), "{url}");
        }
        let e = plain("HTTPS://Tole.Example.COM:8443/tole/", false).unwrap();
        assert_eq!(e.scheme(), Scheme::Https);
        assert_eq!(e.to_url(), "https://tole.example.com:8443/tole");
    }

    #[test]
    fn plain_http_is_for_loopback_only_unless_the_server_is_trusted() {
        for ok in [
            "http://127.0.0.1:8080",
            "http://localhost:1/",
            "http://[::1]:9",
            "http://127.0.0.1",
        ] {
            let e = plain(ok, false).unwrap();
            assert!(e.is_loopback(), "{ok}");
        }
        for host in [
            "http://10.0.0.5:8080",
            "http://example.com",
            "http://[fd00::1]:1",
            "http://tailnet-box:8080",
        ] {
            assert!(
                matches!(
                    plain(host, false),
                    Err(EndpointError::PlainHttp {
                        why: PlainHttp::NotTrusted,
                        ..
                    })
                ),
                "{host}"
            );
            assert!(plain(host, true).is_ok(), "{host} trusted");
        }
    }

    #[test]
    fn a_public_ip_is_refused_over_plain_http_even_when_trusted() {
        for public in [
            "http://203.0.113.9:1",
            "http://8.8.8.8",
            "http://[2001:db8::1]:1",
            "http://100.128.0.1",
        ] {
            assert!(
                matches!(
                    plain(public, true),
                    Err(EndpointError::PlainHttp {
                        why: PlainHttp::PublicAddress,
                        ..
                    })
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
        ] {
            assert!(plain(private, true).is_ok(), "{private}");
        }
    }

    #[test]
    fn a_credential_in_the_address_is_refused_everywhere() {
        for url in [
            "https://user:secret@tole.example.com",
            "http://token@127.0.0.1:1",
        ] {
            assert_eq!(
                plain(url, true).unwrap_err(),
                EndpointError::CredentialInUrl,
                "{url}"
            );
        }
    }

    #[test]
    fn malformed_addresses_are_refused() {
        let cases = [
            ("", EndpointError::Empty),
            ("   ", EndpointError::Empty),
            ("ftp://x.example", EndpointError::Scheme),
            ("tole.example.com", EndpointError::Scheme),
            ("https://", EndpointError::Host),
            ("https://[::1", EndpointError::Host),
            ("https://[tole.example]:1", EndpointError::Host),
            ("https://[fe80::1%eth0]", EndpointError::Host),
            ("https://0x08080808", EndpointError::Host),
            ("https://8.8.8", EndpointError::Host),
            ("https://134744072", EndpointError::Host),
            ("https://-bad.example", EndpointError::Host),
            ("https://x.example:0", EndpointError::Port),
            ("https://x.example:99999", EndpointError::Port),
            ("https://x.example:+80", EndpointError::Port),
            ("https://x.example/a?b=1", EndpointError::QueryOrFragment),
            ("https://x.example/#top", EndpointError::QueryOrFragment),
            ("https://x.example/a b", EndpointError::Path),
        ];
        for (url, want) in cases {
            assert_eq!(plain(url, true).unwrap_err(), want, "{url:?}");
        }
    }

    #[test]
    fn an_ipv6_address_is_printed_in_brackets() {
        let e = plain("https://[2001:DB8::1]:9443", false).unwrap();
        assert_eq!(e.to_url(), "https://[2001:db8::1]:9443");
        assert_eq!(e.host(), "2001:db8::1");
    }
}
