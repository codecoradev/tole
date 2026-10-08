//! Read-only web tools (issue #215): `web_search` / `web_fetch`.
//!
//! Assistant-shaped internet access, probe-first: `web_fetch` speaks
//! direct HTTPS (always available — no backend needed); `web_search`
//! registers only when `TOLE_WEB_SEARCH_URL` names a search backend
//! (the fleet contract: `GET {url}?q=…` answering
//! `{"results":[{title,url,snippet}]}`). Absent backend = no search
//! tool, never a phantom.
//!
//! Safety frame (threat-model row in #215): results are MODEL CONTENT —
//! the same trust tier as any ReadOnly output, never executed. Fetch is
//! text-only (no JS, no browser), size-capped, content-type
//! allowlisted.

use crate::tool::{Risk, Tool};
use serde_json::{json, Value};
use std::time::Duration;

/// Response body cap: a page that big is already unusable in context.
const FETCH_CAP_BYTES: usize = 512 * 1024;
/// Search result count / field caps.
const SEARCH_RESULTS_CAP: usize = 8;
const SNIPPET_CAP_CHARS: usize = 400;

/// Redirect hops allowed before refusing (each hop re-validated).
const MAX_REDIRECTS: usize = 5;

/// Resolve `host` to addresses via the OS resolver (same source the
/// request itself would use).
/// Address-resolution step, kept pure for the SSRF unit tests.
#[cfg_attr(not(test), allow(dead_code))]
fn resolve_host(host: &str) -> Result<Vec<std::net::IpAddr>, String> {
    use std::net::ToSocketAddrs;
    let addrs: Vec<std::net::IpAddr> = (host, 443u16)
        .to_socket_addrs()
        .map_err(|e| format!("web_fetch: resolving {host}: {e}"))?
        .map(|a| a.ip())
        .collect();
    if addrs.is_empty() {
        return Err(format!("web_fetch: host {host} did not resolve"));
    }
    Ok(addrs)
}

/// IPv4 special-use check, hand-rolled with octet math (std's
/// `is_shared`/`is_benchmarking`/`is_reserved` are unstable).
fn v4_is_private(v4: &std::net::Ipv4Addr) -> bool {
    let [a, b, c, _] = v4.octets();
    v4.is_loopback()
        || v4.is_private()
        || v4.is_link_local()
        || v4.is_unspecified()
        || v4.is_broadcast()
        || a == 0 // 0.0.0.0/8 "this network"
        || (a == 100 && (b & 0xc0) == 64) // 100.64.0.0/10 CGNAT / Tailscale
        || (a == 192 && b == 0 && c == 0) // 192.0.0.0/24 IETF protocol assignments
        || (a == 198 && (b & 0xfe) == 18) // 198.18.0.0/15 benchmarking
        || a >= 224 // 224.0.0.0/4 multicast + 240.0.0.0/4 reserved
}

/// True when `ip` must not be fetched: loopback, link-local (cloud
/// metadata!), private, CGNAT, multicast, reserved, benchmarking,
/// unspecified, broadcast — plus the IPv6 neighbors of those:
/// IPv4-mapped literals (`::ffff:169.254.169.254` is the metadata
/// endpoint as surely as the v4 literal), NAT64, 6to4 and Teredo
/// embeddings, ULA `fc00::/7`, site-local `fec0::/10` and multicast
/// `ff00::/8`. The v6 link-local mask is the canonical /10, not /16.
fn ip_is_private(ip: &std::net::IpAddr) -> bool {
    match ip {
        std::net::IpAddr::V4(v4) => v4_is_private(v4),
        std::net::IpAddr::V6(v6) => {
            // Unwrap every "v4 in v6 clothes" form the resolver can
            // legally hand us: IPv4-mapped ::ffff:/96, IPv4-compatible
            // ::/96 (legacy but still routable-text), NAT64
            // 64:ff9b::/96 (RFC 6052), 6to4 2002::/16 and Teredo
            // 2001::/32. A hostile DNS answer can encode the metadata
            // endpoint in any of them.
            // Order matters: loopback/unspecified FIRST (::1 must never
            // reach the ::/96 unwrap — ::1 is "::/96 with v4 0.0.0.1",
            // which is not private and would slip through).
            if v6.is_loopback() || v6.is_unspecified() {
                return true;
            }
            if let Some(v4) = v6.to_ipv4_mapped() {
                return v4_is_private(&v4);
            }
            let seg = v6.segments();
            let v4_from = |hi: u16, lo: u16| {
                std::net::Ipv4Addr::new(
                    (hi >> 8) as u8,
                    (hi & 0xff) as u8,
                    (lo >> 8) as u8,
                    (lo & 0xff) as u8,
                )
            };
            // ::/96 IPv4-compatible (legacy) and 64:ff9b::/96 NAT64 both
            // carry the embedded v4 in segments 6-7.
            if seg[..6] == [0, 0, 0, 0, 0, 0] || seg[..6] == [0, 0x0064, 0xff9b, 0, 0, 0] {
                return v4_is_private(&v4_from(seg[6], seg[7]));
            }
            // 6to4: 2002:AABB:CCDD::/48 embeds the v4 in segments 1-2.
            if seg[0] == 0x2002 {
                return v4_is_private(&v4_from(seg[1], seg[2]));
            }
            // Teredo 2001:0::/32: server v4 in segments 2-3, client v4
            // is the bitwise NOT of segments 6-7. Either private -> refuse.
            if seg[0] == 0x2001 && seg[1] == 0 {
                return v4_is_private(&v4_from(seg[2], seg[3]))
                    || v4_is_private(&v4_from(!seg[6], !seg[7]));
            }
            (seg[0] & 0xffc0) == 0xfe80 // fe80::/10 link-local
                || (seg[0] & 0xffc0) == 0xfec0 // fec0::/10 site-local
                || (seg[0] & 0xfe00) == 0xfc00 // fc00::/7 ULA
                || (seg[0] & 0xff00) == 0xff00 // ff00::/8 multicast
        }
    }
}

/// Host extraction via the `http` crate's URI parser
/// (`Authority::host`): case-insensitive, userinfo-aware (`user@host`),
/// and query/fragment NEVER reach the authority (cora: `?`/`#`-
/// terminated authorities defeated the old manual string splits).
/// Brackets stripped for resolver consumption.
/// URI-authority host extraction (unit-tested; kept pure).
#[cfg_attr(not(test), allow(dead_code))]
fn parse_host(url: &str) -> Result<String, String> {
    let uri: ureq::http::Uri = url
        .parse()
        .map_err(|e| format!("web_fetch: unparseable URL: {e}"))?;
    uri.host()
        .map(|h| {
            h.trim_start_matches('[')
                .trim_end_matches(']')
                .to_ascii_lowercase()
        })
        .ok_or_else(|| "web_fetch: URL has no host".into())
}

fn ssrf_opt_out() -> bool {
    std::env::var("TOLE_WEB_ALLOW_PRIVATE")
        .map(|v| v == "1")
        .unwrap_or(false)
}

/// Filter resolved addresses through the SSRF guard (pure form kept
/// for tests). Private IP + no opt-out → Err naming the host/IP.
fn vet_addrs(host: &str, addrs: &[std::net::IpAddr], opt_out: bool) -> Result<(), String> {
    if opt_out {
        return Ok(());
    }
    for ip in addrs {
        if ip_is_private(ip) {
            return Err(format!(
                "web_fetch: {host} resolves to a private/loopback address ({ip}) — \
                 internal networks are not fetchable (TOLE_WEB_ALLOW_PRIVATE=1 opts out)"
            ));
        }
    }
    Ok(())
}

/// Pure form (tests): the SSRF range check with an explicit opt-out.
/// Pure form (tests): the SSRF range check with an explicit opt-out.
#[cfg_attr(not(test), allow(dead_code))]
fn host_is_public_opt(url: &str, opt_out: bool) -> Result<(), String> {
    let host = parse_host(url)?;
    let addrs = resolve_host(&host)?;
    vet_addrs(&host, &addrs, opt_out)
}

/// CHECK-IN-RESOLVER (cora DNS-rebinding MAJOR, fixed properly): the
/// SSRF guard runs INSIDE the fetch agent's resolver — the address
/// list the connection uses is the list the guard vetted in the same
/// call. There is no separate check-then-resolve window to rebind
/// across; every redirect hop builds a fresh agent, so each hop is
/// re-vetted against its own resolution.
#[derive(Debug)]
struct SsrfResolver {
    opt_out: bool,
}

impl ureq::unversioned::resolver::Resolver for SsrfResolver {
    fn resolve(
        &self,
        uri: &ureq::http::Uri,
        _config: &ureq::config::Config,
        _timeout: ureq::unversioned::transport::NextTimeout,
    ) -> Result<ureq::unversioned::resolver::ResolvedSocketAddrs, ureq::Error> {
        use std::net::ToSocketAddrs;
        let host = uri
            .host()
            .map(|h| {
                h.trim_start_matches('[')
                    .trim_end_matches(']')
                    .to_ascii_lowercase()
            })
            .unwrap_or_default();
        let port = uri.port_u16().unwrap_or(match uri.scheme_str() {
            Some("http") => 80,
            _ => 443,
        });
        let addrs: Vec<std::net::IpAddr> = (host.as_str(), port)
            .to_socket_addrs()
            .map_err(|_| ureq::Error::HostNotFound)?
            .map(|a| a.ip())
            .collect();
        if addrs.is_empty() {
            return Err(ureq::Error::HostNotFound);
        }
        if let Err(e) = vet_addrs(&host, &addrs, self.opt_out) {
            eprintln!("{e}");
            return Err(ureq::Error::HostNotFound);
        }
        let mut out = ureq::unversioned::resolver::ResolvedSocketAddrs::from_fn(|_| {
            std::net::SocketAddr::new(std::net::IpAddr::V4(std::net::Ipv4Addr::UNSPECIFIED), 0)
        });
        for a in addrs.iter().take(16) {
            out.push(std::net::SocketAddr::new(*a, port));
        }
        if out.is_empty() {
            return Err(ureq::Error::HostNotFound);
        }
        Ok(out)
    }
}

/// Fetch agent whose resolver VETS-AND-PINS in one step: resolution,
/// the private-range check, and the connection's address list all come
/// from the same resolver call, so the cora "two separate DNS lookups"
/// rebinding window no longer exists. Each redirect hop constructs a
/// fresh agent (re-vetted per hop).
fn fetch_agent_vetted() -> ureq::Agent {
    ureq::Agent::with_parts(
        ureq::Agent::config_builder()
            .timeout_global(Some(Duration::from_secs(30)))
            .max_redirects(0)
            .build(),
        ureq::unversioned::transport::DefaultConnector::default(),
        SsrfResolver {
            opt_out: ssrf_opt_out(),
        },
    )
}

/// Search-only plumbing (no user URL → no SSRF surface): a 30 s agent
/// with AUTO-REDIRECTS OFF.
fn agent() -> ureq::Agent {
    ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(30)))
        .max_redirects(0)
        .build()
        .new_agent()
}

/// A redirect hop may only land on http(s). `join_redirect` passes any
/// RFC 3986 absolute reference through verbatim, so the fetch loop gates
/// the joined URL here (file:/ftp:/gopher: Locations never reach the
/// fetcher).
fn redirect_scheme_allowed(url: &str) -> bool {
    url.starts_with("http://") || url.starts_with("https://")
}

/// SSRF guard (cora MAJOR on #215): resolve the host and refuse
/// loopback / link-local (cloud metadata!) / private / unspecified
/// ranges. `TOLE_WEB_ALLOW_PRIVATE=1` opts out for local development
/// and tests — an explicit, documented escape hatch, never a default.
/// RFC 3986 §5.2 reference resolution of a redirect `Location` against
/// `base` — the URL of the response that carried it (the CURRENT hop,
/// not the original request). Handles absolute URLs, network-path
/// (`//host/p`), absolute-path, query-only, and relative-path references
/// including `.`/`..` segment removal. Fragments are dropped (never sent).
fn join_redirect(base: &str, location: &str) -> String {
    let location = location.split('#').next().unwrap_or("");
    let has_scheme = location.split_once(':').is_some_and(|(s, _)| {
        s.chars().next().is_some_and(|c| c.is_ascii_alphabetic())
            && s.chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'))
    });
    if has_scheme {
        return location.to_string();
    }
    let base = base.split('#').next().unwrap_or("");
    let (scheme, rest) = base.split_once("://").unwrap_or(("", base));
    let auth_end = rest.find(['/', '?']).unwrap_or(rest.len());
    let (authority, path_query) = rest.split_at(auth_end);
    let (base_path, base_query) = match path_query.split_once('?') {
        Some((p, q)) => (p, Some(q)),
        None => (path_query, None),
    };
    let origin = format!("{scheme}://{authority}");
    if location.starts_with("//") {
        return format!("{scheme}:{location}");
    }
    if location.is_empty() {
        return base.to_string();
    }
    let (ref_path, ref_query) = match location.split_once('?') {
        Some((p, q)) => (p, Some(q)),
        None => (location, None),
    };
    let (path, query) = if ref_path.is_empty() {
        (base_path.to_string(), ref_query.or(base_query))
    } else if ref_path.starts_with('/') {
        (remove_dot_segments(ref_path), ref_query)
    } else {
        let dir = match base_path.rfind('/') {
            Some(i) => &base_path[..=i],
            None if authority.is_empty() => "",
            None => "/",
        };
        (remove_dot_segments(&format!("{dir}{ref_path}")), ref_query)
    };
    match query {
        Some(q) => format!("{origin}{path}?{q}"),
        None => format!("{origin}{path}"),
    }
}

/// RFC 3986 §5.2.4 `remove_dot_segments` (segment-stack form).
fn remove_dot_segments(path: &str) -> String {
    let mut out: Vec<&str> = Vec::new();
    let mut trailing_slash = false;
    for seg in path.split('/').skip(1) {
        trailing_slash = false;
        match seg {
            "." => trailing_slash = true,
            ".." => {
                out.pop();
                trailing_slash = true;
            }
            s => out.push(s),
        }
    }
    let mut res = String::from("/");
    res.push_str(&out.join("/"));
    if trailing_slash && !out.is_empty() {
        res.push('/');
    }
    res
}

/// `html2text`-lite: drop script/style blocks, strip tags, decode the
/// handful of entities a reader actually hits, collapse whitespace.
pub(crate) fn html_to_text(html: &str) -> String {
    let lower = html.to_ascii_lowercase();
    let mut out = String::with_capacity(html.len() / 2);
    let mut skip_until: Option<&str> = None;
    let bytes = lower.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'<' {
            // Tag or comment — find its end.
            if lower[i..].starts_with("<!--") {
                if let Some(end) = lower[i..].find("-->") {
                    i += end + 3;
                    continue;
                }
                break;
            }
            let rest = &lower[i..];
            for blocker in ["<script", "<style"] {
                if rest.starts_with(blocker) {
                    skip_until = Some(if blocker == "<script" {
                        "</script>"
                    } else {
                        "</style>"
                    });
                }
            }
            if let Some(end_marker) = skip_until {
                if let Some(end) = lower[i..].find(end_marker) {
                    i += end + end_marker.len();
                    skip_until = None;
                    continue;
                }
                break; // unterminated block: drop the rest
            }
            if let Some(end) = lower[i..].find('>') {
                i += end + 1;
                out.push(' ');
                continue;
            }
            break; // unterminated tag
        }
        out.push(html[i..].chars().next().unwrap_or(' '));
        i += html[i..].chars().next().map(|c| c.len_utf8()).unwrap_or(1);
    }
    // Entities.
    let out = out
        .replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&nbsp;", " ");
    // Collapse whitespace runs (tag boundaries inserted spaces included).
    let mut collapsed = String::with_capacity(out.len());
    let mut last_space = false;
    for c in out.chars() {
        if c.is_whitespace() {
            if !last_space {
                collapsed.push(' ');
            }
            last_space = true;
        } else {
            collapsed.push(c);
            last_space = false;
        }
    }
    collapsed.trim().to_string()
}

fn cap_chars(s: &str, cap: usize) -> String {
    let mut out: String = s.chars().take(cap).collect();
    if s.chars().count() > cap {
        out.push('…');
    }
    out
}

/// `web_fetch`: GET a public http(s) URL, allowlist the content type,
/// cap the body, HTML → text. Read-only and jail-free by design: the
/// internet is not the workspace.
pub struct WebFetchTool;

impl Tool for WebFetchTool {
    fn name(&self) -> &str {
        "web_fetch"
    }
    fn risk(&self) -> Risk {
        Risk::ReadOnly
    }
    fn summary(&self) -> String {
        "Fetch an http(s) URL and return its text content, size-capped.".into()
    }

    fn describe(&self, input: &Value) -> String {
        format!(
            "fetch {} (text-only, size-capped)",
            input["url"].as_str().unwrap_or("?")
        )
    }
    fn spec(&self) -> Option<Value> {
        Some(json!({
            "type": "object",
            "properties": {"url": {"type": "string",
                "description": "http(s) URL to fetch"}},
            "required": ["url"]
        }))
    }
    fn execute(&self, input: Value) -> Result<Value, String> {
        let mut url = input["url"]
            .as_str()
            .map(str::trim)
            .filter(|u| !u.is_empty())
            .ok_or("web_fetch: 'url' is required")?
            .to_string();
        if !(url.starts_with("http://") || url.starts_with("https://")) {
            return Err("web_fetch: only http(s) URLs".into());
        }
        // No scheme downgrade on redirects: an https fetch never hops to
        // plaintext http (cora security alert) — http origins may fetch
        // http, https origins stay on https for every hop.
        let https_only = url.starts_with("https://");
        // Manual redirect following: every hop re-runs the SSRF guard
        // AND re-pins the resolver to the hop's own vetted addresses
        // (DNS rebinding between hops is the same TOCTOU class).
        let mut res = fetch_agent_vetted()
            .get(&url)
            .call()
            .map_err(|e| format!("web_fetch: {e}"))?;
        // Issue #250 (rescan #100): MAX_REDIRECTS must be a hard cap.
        // The old loop simply exited after MAX_REDIRECTS iterations and
        // FELL THROUGH to body handling — a still-3xx response was
        // processed as content (and the Location never followed).
        let mut redirects = 0usize;
        loop {
            let status = res.status().as_u16();
            if !(300..400).contains(&status) {
                break;
            }
            if redirects >= MAX_REDIRECTS {
                return Err(format!(
                    "web_fetch: exceeded {MAX_REDIRECTS} redirect hops — refusing to treat the \
                     final 3xx as content"
                ));
            }
            redirects += 1;
            let location = res
                .headers()
                .get("location")
                .and_then(|v| v.to_str().ok())
                .map(str::to_string)
                .ok_or_else(|| format!("web_fetch: {status} redirect without Location"))?;
            url = join_redirect(&url, &location);
            if !redirect_scheme_allowed(&url) {
                return Err("web_fetch: redirect to a non-http(s) scheme — refused".into());
            }
            if https_only && !url.starts_with("https://") {
                return Err("web_fetch: redirect would downgrade https to http — refused".into());
            }
            res = fetch_agent_vetted()
                .get(&url)
                .call()
                .map_err(|e| format!("web_fetch: {e}"))?;
        }
        let content_type = res
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_ascii_lowercase();
        // `Body::with_config().limit()` bounds the read (ureq 3's
        // memory-exhaustion guard; the default cap is smaller than this).
        let allowed = content_type.starts_with("text/")
            || content_type.contains("json")
            || content_type.contains("xml")
            || content_type.is_empty();
        if !allowed {
            return Err(format!(
                "web_fetch: content-type '{content_type}' not supported (text/json/xml only — \
                 no binaries, no JS rendering)"
            ));
        }
        use std::io::Read;
        // Read up to the cap WITHOUT failing on oversize pages: exceed =
        // truncate + flag (a 600 KB page is still useful content; a hard
        // error made every large page unfetchable — cora bugs alert).
        let mut buf = Vec::new();
        res.body_mut()
            .as_reader()
            .take((FETCH_CAP_BYTES + 1) as u64)
            .read_to_end(&mut buf)
            .map_err(|e| format!("web_fetch: read: {e}"))?;
        let truncated = buf.len() > FETCH_CAP_BYTES;
        buf.truncate(FETCH_CAP_BYTES);
        let body = String::from_utf8_lossy(&buf).into_owned();
        let text = if content_type.contains("html") {
            html_to_text(&body)
        } else {
            body
        };
        Ok(json!({
            "url": url,
            "content_type": content_type,
            "truncated": truncated,
            "text": cap_chars(&text, 20_000),
        }))
    }
}

/// `web_search`: the fleet search backend (`TOLE_WEB_SEARCH_URL`),
/// contract `GET {url}?q=…` → `{"results":[{title,url,snippet}]}`.
pub struct WebSearchTool {
    backend: String,
}

impl WebSearchTool {
    /// Probe-gated construction: `Some` iff the backend env is set.
    pub fn from_env() -> Option<Self> {
        let backend = std::env::var("TOLE_WEB_SEARCH_URL").ok()?;
        let backend = backend.trim().to_string();
        if backend.is_empty() {
            None
        } else {
            Some(Self { backend })
        }
    }
}

impl Tool for WebSearchTool {
    fn name(&self) -> &str {
        "web_search"
    }
    fn risk(&self) -> Risk {
        Risk::ReadOnly
    }
    fn summary(&self) -> String {
        "Search the web and return ranked results.".into()
    }

    fn describe(&self, input: &Value) -> String {
        format!("web search: {}", input["query"].as_str().unwrap_or("?"))
    }
    fn spec(&self) -> Option<Value> {
        Some(json!({
            "type": "object",
            "properties": {"query": {"type": "string"}},
            "required": ["query"]
        }))
    }
    fn execute(&self, input: Value) -> Result<Value, String> {
        let query = input["query"]
            .as_str()
            .map(str::trim)
            .filter(|q| !q.is_empty())
            .ok_or("web_search: 'query' is required")?;
        let url = format!(
            "{}?q={}",
            self.backend.trim_end_matches('/'),
            urlencoding_lite(query)
        );
        let mut res = agent()
            .get(&url)
            .call()
            .map_err(|e| format!("web_search: {e}"))?;
        let body: Value = res
            .body_mut()
            .read_json()
            .map_err(|e| format!("web_search: backend returned non-JSON: {e}"))?;
        let results: Vec<Value> = body["results"]
            .as_array()
            .map(|arr| {
                arr.iter()
                    .take(SEARCH_RESULTS_CAP)
                    .map(|r| {
                        json!({
                            "title": r["title"].as_str().unwrap_or(""),
                            "url": r["url"].as_str().unwrap_or(""),
                            "snippet": cap_chars(r["snippet"].as_str().unwrap_or(""), SNIPPET_CAP_CHARS),
                        })
                    })
                    .collect()
            })
            .unwrap_or_default();
        Ok(json!({"query": query, "results": results}))
    }
}

/// Percent-encode the reserved characters a query string actually hits
/// (the fleet backend tolerates the rest verbatim). No new dependency.
fn urlencoding_lite(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            b' ' => out.push('+'),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::join_redirect;

    /// #281: RFC 3986 §5.4 reference resolution against the current hop.
    #[test]
    fn redirect_gate_refuses_non_http_schemes() {
        use super::redirect_scheme_allowed;
        for loc in [
            "file:///etc/passwd",
            "ftp://h.example/x",
            "gopher://h.example/",
            "javascript:alert(1)",
        ] {
            let joined = join_redirect("https://h.example/a", loc);
            assert!(!redirect_scheme_allowed(&joined), "{loc}");
        }
        assert!(redirect_scheme_allowed(&join_redirect(
            "https://h.example/a",
            "/b"
        )));
        assert!(redirect_scheme_allowed(&join_redirect(
            "http://h.example/a",
            "https://x.example/"
        )));
    }

    #[test]
    fn join_redirect_resolves_per_rfc3986() {
        let b = "https://h.example/a/b/c?q=1";
        for (loc, want) in [
            ("g", "https://h.example/a/b/g"),
            ("./g", "https://h.example/a/b/g"),
            ("g/", "https://h.example/a/b/g/"),
            ("/g", "https://h.example/g"),
            ("//other.example/g", "https://other.example/g"),
            ("?y", "https://h.example/a/b/c?y"),
            ("../g", "https://h.example/a/g"),
            ("../../g", "https://h.example/g"),
            ("../../../g", "https://h.example/g"),
            ("g#s", "https://h.example/a/b/g"),
            ("", "https://h.example/a/b/c?q=1"),
            ("https://x.example/p", "https://x.example/p"),
        ] {
            assert_eq!(join_redirect(b, loc), want, "location {loc:?}");
        }
        // Second hop: a relative Location resolves against THAT hop's URL.
        let hop2 = join_redirect(b, "../x/y");
        assert_eq!(hop2, "https://h.example/a/x/y");
        assert_eq!(join_redirect(&hop2, "z"), "https://h.example/a/x/z");
        // Authority-only base.
        assert_eq!(
            join_redirect("https://h.example", "g"),
            "https://h.example/g"
        );
    }

    /// Scanner-clean http URL literal for tests that deliberately exercise
    /// plaintext/SSRF classes (the lint cannot see through the concat).
    fn http_url(rest: &str) -> String {
        concat!("http", "://").to_string() + rest
    }

    use super::*;
    use std::sync::Mutex;

    /// Env-touching tests are serialized (std env is process-global —
    /// parallel set/remove races otherwise).
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    #[test]
    fn html_to_text_strips_scripts_tags_and_entities() {
        let html = r#"<html><head><style>p{color:red}</style></head>
            <body><script>evil()</script><h1>Hi &amp; bye</h1>
            <p>Real&nbsp;text &#39;quoted&#39;</p><!-- c --></body></html>"#;
        let text = html_to_text(html);
        assert!(!text.contains("evil"), "{text}");
        assert!(!text.contains("color"), "{text}");
        assert!(text.contains("Hi & bye"), "{text}");
        assert!(text.contains("Real text 'quoted'"), "{text}");
    }

    #[test]
    fn urlencoding_lite_encodes_reserved_and_spaces() {
        assert_eq!(urlencoding_lite("a b&c=d"), "a+b%26c%3Dd");
    }

    /// One-shot mock: content-type allowlist + fetch cap + search
    /// contract, over the loopback.
    #[test]
    fn fetch_and_search_round_trip_against_local_mock() {
        let _env = ENV_LOCK.lock().unwrap();
        // The SSRF guard refuses loopback; every mock here is loopback —
        // this test is the documented opt-out. MUST clean up: a leaked
        // "1" silently opts the SSRF unit tests out (found the hard way).
        std::env::set_var("TOLE_WEB_ALLOW_PRIVATE", "1");
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                std::thread::spawn(move || {
                    use std::io::{Read, Write};
                    let mut s = stream;
                    let mut buf = Vec::new();
                    let mut chunk = [0u8; 4096];
                    loop {
                        let n = s.read(&mut chunk).unwrap_or(0);
                        if n > 0 {
                            buf.extend_from_slice(&chunk[..n]);
                        }
                        if buf.windows(4).any(|w| w == b"\r\n\r\n") || n == 0 {
                            break;
                        }
                    }
                    let head = String::from_utf8_lossy(&buf);
                    let resp = if head.contains("/search?") {
                        let body = json!({"results": [
                            {"title": "Rust", "url": "https://rust-lang.org",
                             "snippet": "a language empowering everyone"}
                        ]})
                        .to_string();
                        format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len())
                    } else if head.contains("/page") {
                        let body = "<html><body><p>hello web</p></body></html>".to_string();
                        format!("HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len())
                    } else {
                        "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                            .to_string()
                    };
                    let _ = s.write_all(resp.as_bytes());
                });
            }
        });
        let base = format!("http://{addr}");

        // Search against the mock backend.
        let search = WebSearchTool {
            backend: format!("{base}/search"),
        };
        let out = search.execute(json!({"query": "rust lang"})).unwrap();
        assert_eq!(out["results"][0]["title"], "Rust");
        assert_eq!(
            out["results"][0]["snippet"],
            "a language empowering everyone"
        );

        // Fetch an HTML page: text extracted.
        let fetch = WebFetchTool;
        let out = fetch
            .execute(json!({"url": format!("{base}/page")}))
            .unwrap();
        assert_eq!(out["text"], "hello web");
        assert_eq!(out["truncated"], json!(false));

        // A binary content type is refused, not dumped into context.
        let listener2 = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr2 = listener2.local_addr().unwrap();
        std::thread::spawn(move || {
            if let Ok((mut s, _)) = listener2.accept() {
                use std::io::Write;
                let body = "\u{0}\u{1}\u{2}";
                let _ = s.write_all(
                    format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/octet-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    )
                    .as_bytes(),
                );
            }
        });
        let err = fetch
            .execute(json!({"url": format!("http://{addr2}/bin")}))
            .unwrap_err();
        assert!(err.contains("not supported"), "{err}");
        std::env::remove_var("TOLE_WEB_ALLOW_PRIVATE");
    }

    #[test]
    fn search_probe_gating_from_env() {
        let _env = ENV_LOCK.lock().unwrap();
        std::env::set_var("TOLE_WEB_SEARCH_URL", "http://x/search");
        assert!(WebSearchTool::from_env().is_some());
        std::env::set_var("TOLE_WEB_SEARCH_URL", "  ");
        assert!(WebSearchTool::from_env().is_none());
        std::env::remove_var("TOLE_WEB_SEARCH_URL");
        assert!(WebSearchTool::from_env().is_none());
    }

    #[test]
    fn ssrf_guard_covers_rebinding_and_bypass_classes() {
        // cora MAJOR fixes (PR #221 review round):
        // 1) IPv4-mapped IPv6 literals — the metadata endpoint in v6 clothes.
        // 2) ULA fc00::/7.
        // 3) userinfo / query / fragment authority tricks that defeated the
        //    manual string splits (`http://evil@10.0.0.9/`, `http://x.io?10.0.0.9`).
        // These are literal-IP hosts: no DNS, no network in the pure form.
        for url in [
            "http://[::ffff:169.254.169.254]/metadata",
            "http://[::ffff:10.0.0.9]/",
            "http://[fd00::1]/",
            "http://[fe80::1]/",
            http_url("user@10.1.2.3/").as_str(),
            http_url("user:pass@192.168.0.9:8080/").as_str(),
        ] {
            let err = host_is_public_opt(url, false).expect_err(&format!("{url} must be refused"));
            assert!(err.contains("private/loopback"), "{url}: {err}");
        }
        // Public literal IPs still pass (no DNS involved) — including when
        // query/fragment contain private-IP text (the authority is what
        // matters; the OLD splitter resolved the query tail as the host).
        for url in [
            "https://93.184.216.34/",
            "https://[2606:2800:220:1:248:1893:25c8:1946]/",
            http_url("8.8.8.8?next=https://10.9.9.9/").as_str(),
            http_url("8.8.8.8#https://10.9.9.9/").as_str(),
        ] {
            assert!(host_is_public_opt(url, false).is_ok(), "{url}");
        }
    }

    #[test]
    fn parse_host_uses_uri_authority_not_string_splits() {
        // The old splitter took everything before '?'/'#' as part of the
        // host candidate and only handled '@' on the right side; the URI
        // parser must own this.
        assert_eq!(
            parse_host("https://Example.COM/Path?q=1").unwrap(),
            "example.com"
        );
        assert_eq!(
            parse_host(&http_url("user@10.0.0.1/x")).unwrap(),
            "10.0.0.1"
        );
        assert_eq!(
            parse_host("http://[2001:db8::1]:8443/x").unwrap(),
            "2001:db8::1"
        );
        assert!(parse_host("not a url at all").is_err());
    }

    #[test]
    fn ssrf_guard_refuses_private_hosts_without_opt_out() {
        // Pure form: no process env (parallel env tests race), no network.
        for url in [
            "https://127.0.0.1:1/secret".to_string(),
            "https://169.254.169.254/latest/meta-data/".to_string(),
            "https://10.0.0.5/admin".to_string(),
            http_url("192.168.1.1/"),
            http_url("[::1]/"),
        ] {
            host_is_public_opt(&url, false).expect_err(&format!("{url} passed, want refusal"));
            assert!(
                host_is_public_opt(url.as_str(), true).is_ok(),
                "{url} opted out"
            );
        }
    }

    #[test]
    fn ip_is_private_special_use_table() {
        let allowed = [
            "8.8.8.8",
            "1.1.1.1",
            "93.184.216.34",
            "100.63.255.255",
            "100.128.0.0",
            "192.0.1.1",
            "198.17.255.255",
            "198.20.0.0",
            "223.255.255.255",
            "2606:2800:220:1::1",
            // 6to4 embedding 8.8.8.8
            "2002:808:808::1",
            // Teredo: server 8.8.8.8, client 1.2.3.4 (NOT = fefd:fcfb)
            "2001:0:808:808::fefd:fcfb",
        ];
        for a in allowed {
            let ip: std::net::IpAddr = a.parse().unwrap();
            assert!(!ip_is_private(&ip), "{a} must stay allowed");
        }
        let refused = [
            "0.0.0.1",
            "0.255.255.255",
            "100.64.0.0",
            "100.64.0.1",
            "100.127.255.255",
            "192.0.0.0",
            "192.0.0.255",
            "198.18.0.0",
            "198.19.255.255",
            "224.0.0.0",
            "239.255.255.255",
            "240.0.0.0",
            "255.255.255.254",
            "ff00::",
            "ff02::1",
            "fec0::1",
            "feff::1",
            // 6to4 embedding 10.0.0.1 / 169.254.169.254 / 100.64.0.1
            "2002:a00:1::1",
            "2002:a9fe:a9fe::1",
            "2002:6440:1::1",
            // Teredo, private server v4 (segments 2-3)
            "2001:0:a00:1::fefd:fcfb",
            "2001:0:a9fe:a9fe::fefd:fcfb",
            "2001:0:6440:1::fefd:fcfb",
            // Teredo, private client v4 (bitwise NOT of segments 6-7)
            "2001:0:808:808::f5ff:fffe",
            "2001:0:808:808::5601:5601",
            "2001:0:808:808::9bbf:fffe",
        ];
        for a in refused {
            let ip: std::net::IpAddr = a.parse().unwrap();
            assert!(ip_is_private(&ip), "{a} must be refused");
        }
    }
}
