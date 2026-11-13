//! The built-in web tools (#625, the `web-builtin` feature): `web_search`
//! over nginjen-lite's keyless engines and `web_fetch` with its HTML
//! converters, on tole's own HTTP client.
//!
//! - **One client.** [`TolHttp`] implements lite's `HttpClient` with lite's
//!   `get_following_redirects` around [`tole_core::web::vetted_get`], one
//!   hop at a time. Every hop goes through tole's SSRF resolver, a hop that
//!   carries a known secret is refused, an https request never hops to
//!   plain http, and a host gets at most one request per second.
//! - **Search.** DuckDuckGo Lite, then Bing, then Brave (lite's fallback
//!   chain: a non-2xx answer moves on). The query is redacted with the
//!   process secrets and capped before it leaves. The engines get lite's
//!   browser-like User-Agent, because they block other agents.
//! - **Fetch.** http(s) only, a URL with a known secret is refused, the
//!   host must be public, robots.txt is honoured (fail-open when it cannot
//!   be read), the content type must be text, JSON or XML, the body is
//!   capped. HTML becomes markdown (default), compact text, plain text, raw
//!   HTML or a link list. Requests carry [`USER_AGENT`].
//! - **Memory.** Per tool: the robots cache (at most 64 origins, lite's cap)
//!   and the pacer (at most [`PACER_HOSTS`] hosts). No threads; every input
//!   and output is capped per call.

use std::cell::Cell;
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use nginjen_lite::content_converter::{convert_html, extract_links_resolved};
use nginjen_lite::http::{get_following_redirects, Hop as LiteHop};
use nginjen_lite::robots_fetch::RobotsChecker;
use nginjen_lite::{
    HttpClient, HttpError, HttpRequest, HttpResponse, OutputFormat, SearchBackend, SearchClient,
};
use serde_json::{json, Value};
use tole_core::secrets::SecretSet;
use tole_core::tool::{Risk, Tool};
use tole_core::web::{vet_public_url, vetted_get, Hop, HopError};

/// The User-Agent of `web_fetch` and its robots.txt requests.
pub const USER_AGENT: &str = concat!(
    "tole/",
    env!("CARGO_PKG_VERSION"),
    " (+https://github.com/codecoradev/tole)"
);
/// Hosts the pacer remembers; the oldest is dropped past this.
pub const PACER_HOSTS: usize = 64;
/// Minimum time between two requests to one host.
pub const HOST_INTERVAL: Duration = Duration::from_secs(1);
/// Body cap of a fetched page (more is truncated and flagged).
pub const FETCH_CAP_BYTES: usize = 512 * 1024;
/// Body cap of a search engine or robots.txt answer.
pub const CLIENT_CAP_BYTES: usize = 2 * 1024 * 1024;
/// Characters of page text returned to the model.
pub const TEXT_CAP_CHARS: usize = 20_000;
/// Links returned by the `links` format.
pub const LINKS_CAP: usize = 200;
/// Search results returned, and their field caps.
pub const RESULTS_CAP: usize = 8;
pub const QUERY_CAP_CHARS: usize = 512;
const TITLE_CAP_CHARS: usize = 200;
const URL_CAP_CHARS: usize = 2_000;
const SNIPPET_CAP_CHARS: usize = 400;

fn cap(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max.saturating_sub(1)).collect();
    out.push('…');
    out
}

/// At most one request per [`HOST_INTERVAL`] per host, remembering at most
/// [`PACER_HOSTS`] hosts.
#[derive(Debug, Default)]
pub struct Pacer {
    last: Mutex<VecDeque<(String, Instant)>>,
}

impl Pacer {
    /// Waits until `host` may be asked again, then records the request.
    pub fn wait(&self, host: &str) {
        let mut last = self.last.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(i) = last.iter().position(|(h, _)| h == host) {
            let (_, at) = last.remove(i).expect("position is in range");
            let since = at.elapsed();
            if since < HOST_INTERVAL {
                std::thread::sleep(HOST_INTERVAL - since);
            }
        }
        if last.len() >= PACER_HOSTS {
            last.pop_front();
        }
        last.push_back((host.to_string(), Instant::now()));
    }

    pub fn hosts(&self) -> usize {
        self.last.lock().unwrap_or_else(|p| p.into_inner()).len()
    }
}

fn host_of(url: &str) -> String {
    url::Url::parse(url)
        .ok()
        .and_then(|u| u.host_str().map(str::to_ascii_lowercase))
        .unwrap_or_default()
}

/// The policy every hop passes before it is fetched (the SSRF check runs in
/// the hop itself): an https request never hops to plain http, and no hop
/// may carry a known secret.
pub fn check_hop(started_https: bool, url: &str, secrets: &SecretSet) -> Result<(), HttpError> {
    if started_https && !url.starts_with("https://") {
        return Err(HttpError::Redirect("https never hops to plain http".into()));
    }
    if matches!(secrets.redact(url), std::borrow::Cow::Owned(_)) {
        return Err(HttpError::InvalidUrl(
            "the URL contains a known secret value".into(),
        ));
    }
    Ok(())
}

/// tole's HTTP client for nginjen-lite (see the module docs).
pub struct TolHttp {
    pacer: Pacer,
    secrets: SecretSet,
    allow_private: bool,
}

impl TolHttp {
    pub fn new(secrets: &SecretSet, allow_private: bool) -> TolHttp {
        TolHttp {
            pacer: Pacer::default(),
            secrets: secrets.clone(),
            allow_private,
        }
    }

    /// Fetches `request`, one vetted hop at a time. `truncate` cuts an
    /// oversized body (and sets `truncated`) instead of failing.
    fn fetch(
        &self,
        request: &HttpRequest,
        truncate: bool,
        truncated: &Cell<bool>,
    ) -> Result<HttpResponse, HttpError> {
        let https = request.url.starts_with("https://");
        get_following_redirects(request, |hop: &LiteHop| {
            check_hop(https, &hop.url, &self.secrets)?;
            self.pacer.wait(&host_of(&hop.url));
            let r = vetted_get(&Hop {
                url: &hop.url,
                headers: &hop.headers,
                timeout: hop.timeout,
                max_bytes: hop.max_bytes,
                allow_private: self.allow_private,
                truncate,
            })
            .map_err(|e| match e {
                HopError::InvalidUrl => HttpError::InvalidUrl(self.secrets.redact(&hop.url).into()),
                HopError::Private(why) => HttpError::Transport(why),
                HopError::TooLarge(limit) => HttpError::TooLarge { limit },
                HopError::Timeout => HttpError::Timeout,
                HopError::Transport(why) => HttpError::Transport(why),
            })?;
            truncated.set(truncated.get() || r.truncated);
            Ok(HttpResponse {
                status: r.status,
                url: hop.url.clone(),
                headers: r.headers,
                body: r.body,
            })
        })
    }
}

impl HttpClient for TolHttp {
    fn get(&self, request: &HttpRequest) -> Result<HttpResponse, HttpError> {
        let mut capped = request.clone();
        capped.max_bytes = capped.max_bytes.min(CLIENT_CAP_BYTES);
        self.fetch(&capped, false, &Cell::new(false))
    }
}

/// `web_search` over the keyless engines.
pub struct BuiltinSearchTool {
    client: SearchClient<Arc<dyn HttpClient>>,
    secrets: SecretSet,
}

impl BuiltinSearchTool {
    pub fn new(http: Arc<dyn HttpClient>, secrets: &SecretSet) -> BuiltinSearchTool {
        BuiltinSearchTool {
            client: SearchClient::new(http),
            secrets: secrets.clone(),
        }
    }
}

fn engine_name(b: SearchBackend) -> &'static str {
    match b {
        SearchBackend::Lite => "duckduckgo-lite",
        SearchBackend::Bing => "bing",
        SearchBackend::Brave => "brave",
        SearchBackend::Meta => "meta",
        SearchBackend::Searxng => "searxng",
    }
}

impl Tool for BuiltinSearchTool {
    fn name(&self) -> &str {
        "web_search"
    }
    fn risk(&self) -> Risk {
        Risk::ReadOnly
    }
    fn summary(&self) -> String {
        "Search the web (DuckDuckGo Lite, then Bing, then Brave) and return ranked results.".into()
    }
    fn describe(&self, input: &Value) -> String {
        format!(
            "web search: {}",
            self.secrets.redact(input["query"].as_str().unwrap_or("?"))
        )
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
        // #595: the query is model-written and goes to third parties.
        let query = cap(&self.secrets.redact(query), QUERY_CAP_CHARS);
        let (results, engine, fallback) = self
            .client
            .search_with_fallbacks(
                &query,
                SearchBackend::Lite,
                &[SearchBackend::Bing, SearchBackend::Brave],
                RESULTS_CAP as u32,
            )
            .map_err(|e| format!("web_search: {}", self.secrets.redact(&e.to_string())))?;
        let results: Vec<Value> = results
            .iter()
            .take(RESULTS_CAP)
            .map(|r| {
                json!({
                    "title": cap(&r.title, TITLE_CAP_CHARS),
                    "url": cap(&r.url, URL_CAP_CHARS),
                    "snippet": cap(&r.description, SNIPPET_CAP_CHARS),
                })
            })
            .collect();
        Ok(json!({
            "query": query,
            "engine": engine_name(engine),
            "fallback": fallback,
            "results": results,
        }))
    }
}

/// `web_fetch` with robots.txt and the HTML converters.
pub struct BuiltinFetchTool {
    http: Arc<TolHttp>,
    robots: RobotsChecker<Arc<TolHttp>>,
}

impl BuiltinFetchTool {
    pub fn new(http: Arc<TolHttp>) -> BuiltinFetchTool {
        BuiltinFetchTool {
            robots: RobotsChecker::new(Arc::clone(&http), USER_AGENT),
            http,
        }
    }
}

fn format_of(name: &str) -> Result<OutputFormat, String> {
    Ok(match name {
        "markdown" => OutputFormat::Markdown,
        "compact" => OutputFormat::Compact,
        "text" => OutputFormat::Content,
        "html" => OutputFormat::RawHtml,
        "links" => OutputFormat::Links,
        other => {
            return Err(format!(
                "web_fetch: unknown format `{other}` (markdown, compact, text, html or links)"
            ))
        }
    })
}

impl Tool for BuiltinFetchTool {
    fn name(&self) -> &str {
        "web_fetch"
    }
    fn risk(&self) -> Risk {
        Risk::ReadOnly
    }
    fn summary(&self) -> String {
        "Fetch an http(s) URL (robots.txt honoured) and return it as markdown (default), compact \
         text, plain text, raw HTML or a link list, size-capped. `compact` is shortest but drops \
         the text of inline bold, italic and code and image alt text."
            .into()
    }
    fn describe(&self, input: &Value) -> String {
        format!(
            "fetch {} ({})",
            self.http
                .secrets
                .redact(input["url"].as_str().unwrap_or("?")),
            input["format"].as_str().unwrap_or("markdown")
        )
    }
    fn spec(&self) -> Option<Value> {
        Some(json!({
            "type": "object",
            "properties": {
                "url": {"type": "string", "description": "http(s) URL to fetch"},
                "format": {"type": "string",
                    "enum": ["markdown", "compact", "text", "html", "links"],
                    "description": "Output format (default markdown)"}
            },
            "required": ["url"]
        }))
    }
    fn execute(&self, input: Value) -> Result<Value, String> {
        let url = input["url"]
            .as_str()
            .map(str::trim)
            .filter(|u| !u.is_empty())
            .ok_or("web_fetch: 'url' is required")?;
        let format_name = input["format"].as_str().unwrap_or("markdown");
        let format = format_of(format_name)?;
        if let std::borrow::Cow::Owned(r) = self.http.secrets.redact(url) {
            return Err(format!(
                "web_fetch: refused, the URL contains a known secret value: {r}"
            ));
        }
        vet_public_url(url, self.http.allow_private)?;
        if !self.robots.is_allowed(url) {
            return Err(format!(
                "web_fetch: robots.txt of {} disallows this URL for tole",
                host_of(url)
            ));
        }
        let request = HttpRequest::get(url)
            .header("User-Agent", USER_AGENT)
            .header(
                "Accept",
                "text/html,application/xhtml+xml,text/plain,application/json;q=0.9,*/*;q=0.5",
            )
            .timeout(Duration::from_secs(30))
            .max_bytes(FETCH_CAP_BYTES);
        let truncated = Cell::new(false);
        let res = self
            .http
            .fetch(&request, true, &truncated)
            .map_err(|e| format!("web_fetch: {}", self.http.secrets.redact(&e.to_string())))?;
        if !res.is_success() {
            return Err(format!("web_fetch: HTTP {}", res.status));
        }
        let content_type = res
            .header("content-type")
            .unwrap_or("")
            .to_ascii_lowercase();
        let allowed = content_type.starts_with("text/")
            || content_type.contains("json")
            || content_type.contains("xml")
            || content_type.is_empty();
        if !allowed {
            return Err(format!(
                "web_fetch: content-type '{content_type}' not supported (text/json/xml only)"
            ));
        }
        let body = res.text();
        let html = content_type.contains("html") || content_type.is_empty();
        let mut out = json!({
            "url": res.url,
            "content_type": content_type,
            "format": format_name,
            "truncated": truncated.get(),
        });
        if format == OutputFormat::Links {
            let links: Vec<Value> = match (html, url::Url::parse(&res.url)) {
                (true, Ok(base)) => extract_links_resolved(&body, &base)
                    .into_iter()
                    .take(LINKS_CAP)
                    .map(|l| json!({"url": cap(&l.url, URL_CAP_CHARS), "text": cap(&l.text, TITLE_CAP_CHARS)}))
                    .collect(),
                _ => Vec::new(),
            };
            out["links"] = json!(links);
            return Ok(out);
        }
        let text = if html {
            convert_html(&body, format)
        } else {
            body
        };
        out["truncated"] = json!(truncated.get() || text.chars().count() > TEXT_CAP_CHARS);
        out["text"] = json!(cap(&text, TEXT_CAP_CHARS));
        Ok(out)
    }
}

#[cfg(test)]
#[path = "web_builtin_tests.rs"]
mod tests;
