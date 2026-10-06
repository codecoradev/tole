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

/// True when `ip` must not be fetched: loopback, link-local (cloud
/// metadata!), private, unspecified, broadcast — plus the IPv6
/// neighbors of those: IPv4-mapped literals (`::ffff:169.254.169.254`
/// is the metadata endpoint as surely as the v4 literal) and ULA
/// `fc00::/7`. The v6 link-local mask is the canonical /10, not /16.
fn ip_is_private(ip: &std::net::IpAddr) -> bool {
    match ip {
        std::net::IpAddr::V4(v4) => {
            v4.is_loopback()
                || v4.is_private()
                || v4.is_link_local()
                || v4.is_unspecified()
                || v4.is_broadcast()
        }
        std::net::IpAddr::V6(v6) => {
            if let Some(v4) = v6.to_ipv4_mapped() {
                return ip_is_private(&std::net::IpAddr::V4(v4));
            }
            let seg = v6.segments();
            v6.is_loopback()
                || v6.is_unspecified()
                || (seg[0] & 0xffc0) == 0xfe80
                || (seg[0] & 0xfe00) == 0xfc00
        }
    }
}

/// Host extraction via the `http` crate's URI parser
/// (`Authority::host`): case-insensitive, userinfo-aware (`user@host`),
/// and query/fragment NEVER reach the authority (cora: `?`/`#`-
/// terminated authorities defeated the old manual string splits).
/// Brackets stripped for resolver consumption.
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

fn host_is_public(url: &str) -> Result<(), String> {
    let opt_out = std::env::var("TOLE_WEB_ALLOW_PRIVATE")
        .map(|v| v == "1")
        .unwrap_or(false);
    host_is_public_opt(url, opt_out)
}

/// Pure form (tests): the SSRF range check with an explicit opt-out.
fn host_is_public_opt(url: &str, opt_out: bool) -> Result<(), String> {
    if opt_out {
        return Ok(());
    }
    let host = parse_host(url)?;
    let addrs = resolve_host(&host)?;
    for ip in &addrs {
        if ip_is_private(ip) {
            return Err(format!(
                "web_fetch: {host} resolves to a private/loopback address ({ip}) — \
                 internal networks are not fetchable (TOLE_WEB_ALLOW_PRIVATE=1 opts out)"
            ));
        }
    }
    Ok(())
}

thread_local! {
    /// Addresses vetted for the NEXT fetch on this thread. The fetch
    /// agent's resolver answers ONLY with these — a DNS rebinding that
    /// flips between the check and the connect cannot re-point the
    /// request into an internal network (cora DNS-rebinding MAJOR).
    static VETTED: std::cell::RefCell<Vec<std::net::IpAddr>> =
        const { std::cell::RefCell::new(Vec::new()) };
}

/// Validate `url` through the SSRF guard, pin its resolved addresses,
/// and return an agent whose resolver answers only with those pins.
/// The pinning consumes the guard's OWN resolution (same OS resolver),
/// closing the check-vs-connect gap. `web_search` has no user URL
/// (its endpoint is the configured backend), so the plain agent
/// suffices there.
fn fetch_agent_vetted(url: &str) -> Result<ureq::Agent, String> {
    host_is_public(url)?;
    let addrs = resolve_host(&parse_host(url)?)?;
    VETTED.with(|v| {
        let mut v = v.borrow_mut();
        v.clear();
        v.extend(addrs);
    });
    Ok(ureq::Agent::with_parts(
        ureq::Agent::config_builder()
            .timeout_global(Some(Duration::from_secs(30)))
            .max_redirects(0)
            .build(),
        ureq::unversioned::transport::DefaultConnector::default(),
        PinningResolver,
    ))
}

/// Resolver answering ONLY with this thread's vetted addresses.
#[derive(Debug)]
struct PinningResolver;

impl ureq::unversioned::resolver::Resolver for PinningResolver {
    fn resolve(
        &self,
        uri: &ureq::http::Uri,
        _config: &ureq::config::Config,
        _timeout: ureq::unversioned::transport::NextTimeout,
    ) -> Result<ureq::unversioned::resolver::ResolvedSocketAddrs, ureq::Error> {
        let _ = uri;
        VETTED.with(|v| {
            let vetted = v.borrow();
            if vetted.is_empty() {
                return Err(ureq::Error::HostNotFound);
            }
            let mut out = ureq::unversioned::resolver::ResolvedSocketAddrs::from_fn(|_| {
                std::net::SocketAddr::new(std::net::IpAddr::V4(std::net::Ipv4Addr::UNSPECIFIED), 0)
            });
            out.truncate(0);
            let port = uri.port_u16().unwrap_or(match uri.scheme_str() {
                Some("http") => 80,
                _ => 443,
            });
            for ip in vetted.iter().take(16) {
                out.push(std::net::SocketAddr::new(*ip, port));
            }
            if out.is_empty() {
                return Err(ureq::Error::HostNotFound);
            }
            Ok(out)
        })
    }
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

/// SSRF guard (cora MAJOR on #215): resolve the host and refuse
/// loopback / link-local (cloud metadata!) / private / unspecified
/// ranges. `TOLE_WEB_ALLOW_PRIVATE=1` opts out for local development
/// and tests — an explicit, documented escape hatch, never a default.
/// Resolve a possibly-relative Location against the previous URL
/// (minimal join: absolute URLs pass through; leading-/ paths join the
/// origin; everything else joins the current directory).
fn join_redirect(base: &str, location: &str) -> String {
    if location.starts_with("http://") || location.starts_with("https://") {
        return location.to_string();
    }
    let origin_end = base
        .split_once("://")
        .map(|(scheme, rest)| scheme.len() + 3 + rest.split('/').next().unwrap_or("").len())
        .unwrap_or(0);
    let origin = &base[..origin_end];
    if let Some(path) = location.strip_prefix('/') {
        format!("{origin}/{path}")
    } else {
        format!("{}/{}", base.trim_end_matches('/'), location)
    }
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
        // Manual redirect following: every hop re-runs the SSRF guard
        // AND re-pins the resolver to the hop's own vetted addresses
        // (DNS rebinding between hops is the same TOCTOU class).
        let mut res = fetch_agent_vetted(&url)?
            .get(&url)
            .call()
            .map_err(|e| format!("web_fetch: {e}"))?;
        for _ in 0..MAX_REDIRECTS {
            let status = res.status().as_u16();
            if !(300..400).contains(&status) {
                break;
            }
            let location = res
                .headers()
                .get("location")
                .and_then(|v| v.to_str().ok())
                .map(str::to_string)
                .ok_or_else(|| format!("web_fetch: {status} redirect without Location"))?;
            url = join_redirect(&url, &location);
            res = fetch_agent_vetted(&url)?
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
        let mut body = String::new();
        use std::io::Read;
        res.body_mut()
            .with_config()
            .limit(FETCH_CAP_BYTES as u64)
            .reader()
            .read_to_string(&mut body)
            .map_err(|e| format!("web_fetch: read: {e}"))?;
        let truncated = body.len() >= FETCH_CAP_BYTES;
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
        // this test is the documented opt-out.
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
        "http://user@10.1.2.3/",
        "http://user:pass@192.168.0.9:8080/",
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
        "http://8.8.8.8?next=http://10.9.9.9/",
        "http://8.8.8.8#http://10.9.9.9/",
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
    assert_eq!(parse_host("http://user@10.0.0.1/x").unwrap(), "10.0.0.1");
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
        "https://127.0.0.1:1/secret",
        "https://169.254.169.254/latest/meta-data/",
        "https://10.0.0.5/admin",
        "http://192.168.1.1/",
        "http://[::1]/",
    ] {
        let err = host_is_public_opt(url, false).unwrap_err();
        assert!(err.contains("private/loopback"), "{url}: {err}");
        assert!(host_is_public_opt(url, true).is_ok(), "{url} opted out");
    }
}
