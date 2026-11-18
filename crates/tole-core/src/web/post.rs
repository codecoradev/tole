//! `vetted_post`: one signed POST to a destination the owner configured
//! (#911, the outbound webhook delivery target; design in
//! `docs/webhook-delivery-design.md` section 5).
//!
//! It is the sibling of `vetted_get` and shares its building blocks: the
//! connection's resolver vets-and-pins the addresses it resolved (no
//! check-then-resolve window), redirects are never followed, the
//! environment proxy is off, and nothing here reads `TOLE_WEB_ALLOW_PRIVATE`:
//! the address policy is per destination and nothing in the environment can
//! widen it.
//!
//! What a caller can rely on:
//! - `https` is required for every host except a *loopback host* (a literal
//!   loopback IP or the name `localhost`) that the policy opts into;
//! - a loopback address is accepted only for such a host and only with
//!   `AddrPolicy::loopback`, so a hostname a hostile DNS owner points at
//!   `127.0.0.1` cannot aim a signed POST at a local port;
//! - every other non-public range (private, CGNAT, link-local including the
//!   cloud metadata address, ULA, multicast, 6to4/Teredo/NAT64 embeddings)
//!   needs `AddrPolicy::private`, and a host with several addresses is
//!   refused if ANY of them is out of policy;
//! - a policy refusal and a DNS failure are different errors
//!   ([`PostError::RefusedAddress`] is terminal, [`PostError::Dns`] may pass);
//! - a 3xx is [`PostError::Redirect`]; the `Location` is never read;
//! - the response body is never read: only the status and a short
//!   `Retry-After` come back, and nothing a receiver says is returned or
//!   printed (log injection, secret echo).

use std::net::IpAddr;
use std::time::Duration;

use super::{ip_is_private, SsrfResolver, Vetting};

/// A request body above this is refused before anything is sent.
pub const MAX_POST_BODY: usize = 16 * 1024;
/// Longest URL accepted.
const MAX_URL_LEN: usize = 2048;
/// Whole-request ceiling; a caller's timeout is clamped to `1..=30` seconds.
const MAX_TIMEOUT: Duration = Duration::from_secs(30);
/// Connect ceiling.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
/// Response headers larger than this are an error (a receiver controls them).
const MAX_RESPONSE_HEADER: usize = 8 * 1024;
/// `Retry-After` is handed back raw, cut to this many characters.
const RETRY_AFTER_CAP: usize = 64;

/// Headers a caller may not set: the framing and credentials of this request
/// are decided here. The signature travels in an `X-Tole-*` header.
const DENIED_HEADERS: &[&str] = &[
    "host",
    "content-length",
    "transfer-encoding",
    "connection",
    "upgrade",
    "te",
    "trailer",
    "cookie",
    "authorization",
    "proxy-authorization",
];

/// Which resolved addresses a destination may reach, beyond public ones.
/// Both default to `false`. Build it with [`AddrPolicy::new`]; the struct can
/// grow a flag without breaking callers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[non_exhaustive]
pub struct AddrPolicy {
    /// Loopback addresses (and the unspecified address, which reaches this
    /// host too), and plain `http`, for a loopback host only.
    pub loopback: bool,
    /// Every other non-public range. Does NOT include loopback.
    pub private: bool,
}

impl AddrPolicy {
    pub const fn new(loopback: bool, private: bool) -> Self {
        AddrPolicy { loopback, private }
    }
}

/// A literal loopback IP (`127.0.0.0/8`, `::1`, or either inside an
/// IPv4-mapped address).
fn ip_is_loopback(ip: &IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => v4.is_loopback(),
        IpAddr::V6(v6) => {
            v6.is_loopback() || v6.to_ipv4_mapped().is_some_and(|v4| v4.is_loopback())
        }
    }
}

/// An address that reaches THIS host: loopback, or the unspecified address
/// (`0.0.0.0`, `::`, `::ffff:0.0.0.0`), which Linux and macOS connect to as
/// the local host. It needs the `loopback` flag like loopback itself; the
/// `private` flag does not unlock it.
fn ip_reaches_this_host(ip: &IpAddr) -> bool {
    ip_is_loopback(ip)
        || match ip {
            IpAddr::V4(v4) => v4.is_unspecified(),
            IpAddr::V6(v6) => {
                v6.is_unspecified() || v6.to_ipv4_mapped().is_some_and(|v4| v4.is_unspecified())
            }
        }
}

/// A *loopback host*: the name `localhost` or a literal loopback IP. `host`
/// is lower-case without brackets, as `Uri::host` gives it.
fn host_is_loopback(host: &str) -> bool {
    host == "localhost" || host.parse::<IpAddr>().is_ok_and(|ip| ip_is_loopback(&ip))
}

impl AddrPolicy {
    /// Whether a connection to `host` may use all of `addrs`.
    pub(super) fn allows(&self, host: &str, addrs: &[IpAddr]) -> bool {
        addrs.iter().all(|ip| {
            if ip_reaches_this_host(ip) {
                self.loopback && host_is_loopback(host)
            } else {
                !ip_is_private(ip) || self.private
            }
        })
    }
}

/// One POST. Build it with [`PostParams::new`] and set the public fields you
/// need; the struct can grow without breaking callers.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct PostParams<'a> {
    /// The configured destination URL and nothing else.
    pub url: &'a str,
    /// At most [`MAX_POST_BODY`] bytes.
    pub body: &'a [u8],
    /// Extra headers (content type, the `X-Tole-*` set). Names in the denied
    /// list and malformed names or values are refused.
    pub headers: &'a [(String, String)],
    /// Whole-request time, clamped to 1 to 30 seconds.
    pub timeout: Duration,
    /// What the destination may reach.
    pub policy: AddrPolicy,
}

impl<'a> PostParams<'a> {
    /// A POST of `body` to `url` with no extra header, a 10 s timeout and the
    /// strictest policy (public `https` only).
    pub fn new(url: &'a str, body: &'a [u8]) -> Self {
        PostParams {
            url,
            body,
            headers: &[],
            timeout: Duration::from_secs(10),
            policy: AddrPolicy::default(),
        }
    }
}

/// What came back. The body is never read.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct PostResponse {
    pub status: u16,
    /// The raw `Retry-After` value (at most 64 characters), if any.
    pub retry_after: Option<String>,
}

/// Why a POST produced no response. Fixed codes only: no URL, header,
/// address or receiver text is carried, so the value is safe to log.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum PostError {
    /// The URL is malformed, too long, has no host or carries credentials.
    InvalidUrl(&'static str),
    /// A header name or value is malformed or not allowed.
    InvalidHeader(&'static str),
    BodyTooLarge,
    /// `http` for a host that is not an allowed loopback host.
    SchemeRefused,
    /// A resolved address is out of the destination's policy. Terminal.
    RefusedAddress,
    /// The name did not resolve. May pass on a later attempt.
    Dns,
    /// A 3xx answer (never followed).
    Redirect(u16),
    Timeout,
    /// Anything else on the wire: `io:<kind>`, `tls`, `connect`, `protocol`.
    Transport(String),
}

impl std::fmt::Display for PostError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PostError::InvalidUrl(why) => write!(f, "invalid_url: {why}"),
            PostError::InvalidHeader(why) => write!(f, "invalid_header: {why}"),
            PostError::BodyTooLarge => f.write_str("body_too_large"),
            PostError::SchemeRefused => f.write_str("scheme_refused"),
            PostError::RefusedAddress => f.write_str("refused_address"),
            PostError::Dns => f.write_str("dns"),
            PostError::Redirect(code) => write!(f, "redirect_refused: {code}"),
            PostError::Timeout => f.write_str("timeout"),
            PostError::Transport(kind) => write!(f, "transport: {kind}"),
        }
    }
}

impl std::error::Error for PostError {}

/// The marker the resolver's refusal carries, so a refusal is told apart
/// from every other I/O error without matching on text.
#[derive(Debug)]
struct Refused;

impl std::fmt::Display for Refused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("refused_address")
    }
}

impl std::error::Error for Refused {}

/// The resolver's answer for an address out of policy.
pub(super) fn refused_address_error() -> ureq::Error {
    ureq::Error::Io(std::io::Error::new(
        std::io::ErrorKind::PermissionDenied,
        Refused,
    ))
}

/// URL checks that need no network: shape, scheme and loopback rule.
fn check_url(url: &str, policy: AddrPolicy) -> Result<(), PostError> {
    if url.len() > MAX_URL_LEN {
        return Err(PostError::InvalidUrl("too long"));
    }
    let uri: ureq::http::Uri = url
        .parse()
        .map_err(|_| PostError::InvalidUrl("unparseable"))?;
    let host = uri
        .host()
        .map(|h| {
            h.trim_start_matches('[')
                .trim_end_matches(']')
                .to_ascii_lowercase()
        })
        .filter(|h| !h.is_empty())
        .ok_or(PostError::InvalidUrl("no host"))?;
    if uri.authority().is_some_and(|a| a.as_str().contains('@')) {
        // A credential in a URL is never allowed: secrets are never inline.
        return Err(PostError::InvalidUrl("credentials in the URL"));
    }
    match uri.scheme_str() {
        Some("https") => {}
        Some("http") if policy.loopback && host_is_loopback(&host) => {}
        Some("http") => return Err(PostError::SchemeRefused),
        _ => return Err(PostError::InvalidUrl("not an http(s) URL")),
    }
    // A literal IP needs no DNS, so it is vetted here; a name is vetted
    // against what it resolves to when the request is made.
    if let Ok(ip) = host.parse::<IpAddr>() {
        if !policy.allows(&host, &[ip]) {
            return Err(PostError::RefusedAddress);
        }
    }
    Ok(())
}

/// The URL rules of [`vetted_post`] on their own, for a caller that wants to
/// refuse a destination when its configuration is loaded rather than when the
/// first POST goes out: shape, no credentials in the URL, `https` unless the
/// host is a loopback host and `policy.loopback` is set. No network.
pub fn check_destination_url(url: &str, policy: AddrPolicy) -> Result<(), PostError> {
    check_url(url, policy)
}

fn check_headers(headers: &[(String, String)]) -> Result<(), PostError> {
    for (name, value) in headers {
        let lower = name.to_ascii_lowercase();
        if DENIED_HEADERS.contains(&lower.as_str()) || lower.starts_with("proxy-") {
            return Err(PostError::InvalidHeader("name not allowed"));
        }
        ureq::http::HeaderName::from_bytes(name.as_bytes())
            .map_err(|_| PostError::InvalidHeader("malformed name"))?;
        ureq::http::HeaderValue::from_str(value)
            .map_err(|_| PostError::InvalidHeader("malformed value"))?;
    }
    Ok(())
}

/// Maps a ureq failure to a fixed code (see [`PostError::Transport`]).
fn classify(e: ureq::Error) -> PostError {
    match e {
        ureq::Error::Timeout(_) => PostError::Timeout,
        ureq::Error::HostNotFound => PostError::Dns,
        ureq::Error::Io(io) if io.get_ref().is_some_and(|inner| inner.is::<Refused>()) => {
            PostError::RefusedAddress
        }
        ureq::Error::Io(io) => PostError::Transport(format!("io:{:?}", io.kind())),
        ureq::Error::ConnectionFailed => PostError::Transport("connect".into()),
        ureq::Error::Tls(_)
        | ureq::Error::Pem(_)
        | ureq::Error::Rustls(_)
        | ureq::Error::TlsRequired => PostError::Transport("tls".into()),
        ureq::Error::Protocol(_) | ureq::Error::Http(_) | ureq::Error::LargeResponseHeader(..) => {
            PostError::Transport("protocol".into())
        }
        _ => PostError::Transport("other".into()),
    }
}

/// A caller's timeout, held to 1 to 30 seconds: zero is neither "wait
/// forever" nor "fail at once".
fn clamp_timeout(t: Duration) -> Duration {
    t.clamp(Duration::from_secs(1), MAX_TIMEOUT)
}

/// POSTs `params.body` and returns the status (see the module docs).
pub fn vetted_post(params: &PostParams<'_>) -> Result<PostResponse, PostError> {
    if params.body.len() > MAX_POST_BODY {
        return Err(PostError::BodyTooLarge);
    }
    check_url(params.url, params.policy)?;
    check_headers(params.headers)?;

    let timeout = clamp_timeout(params.timeout);
    let config = ureq::Agent::config_builder()
        .timeout_global(Some(timeout))
        .timeout_connect(Some(CONNECT_TIMEOUT.min(timeout)))
        .max_redirects(0)
        .http_status_as_error(false)
        .max_response_header_size(MAX_RESPONSE_HEADER)
        .user_agent(format!("tole/{}", env!("CARGO_PKG_VERSION")))
        .proxy(None)
        .build();
    // A fresh agent per call: no connection, cookie or state is shared
    // between destinations.
    let agent = ureq::Agent::with_parts(
        config,
        ureq::unversioned::transport::DefaultConnector::default(),
        SsrfResolver {
            vetting: Vetting::Post(params.policy),
        },
    );
    let mut req = agent.post(params.url);
    for (name, value) in params.headers {
        req = req.header(name.as_str(), value.as_str());
    }
    let res = req.send(params.body).map_err(classify)?;
    let status = res.status().as_u16();
    if (300..400).contains(&status) {
        return Err(PostError::Redirect(status));
    }
    let retry_after = res
        .headers()
        .get("retry-after")
        .and_then(|v| v.to_str().ok())
        .map(|v| v.chars().take(RETRY_AFTER_CAP).collect());
    // The body is dropped unread: the connection closes with the response.
    Ok(PostResponse {
        status,
        retry_after,
    })
}

#[cfg(test)]
#[path = "post_tests.rs"]
mod tests;
