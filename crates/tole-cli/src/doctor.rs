//! `tole doctor` — tiered provider diagnostics (#378).
//!
//! One command answers "is provider/model X actually usable?":
//!
//! - **offline** — wiring only (config resolves, base URL shape, key
//!   present). No network.
//! - **catalog** — live `GET /models`; the configured model must be served
//!   (when the gateway lists models at all).
//!
//! Tiers are cumulative and stop at the first blocker, which carries the
//! command that fixes it.

use tole_core::openai::{fetch_model_ids, OpenAiConfig, ENV_PREFIXES};

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Tier {
    Offline,
    Catalog,
}

impl Tier {
    pub fn as_str(self) -> &'static str {
        match self {
            Tier::Offline => "offline",
            Tier::Catalog => "catalog",
        }
    }

    pub fn parse(s: &str) -> Option<Tier> {
        match s {
            "offline" => Some(Tier::Offline),
            "catalog" => Some(Tier::Catalog),
            _ => None,
        }
    }
}

/// The first thing that stopped a run, with the command that fixes it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Blocker {
    pub tier: Tier,
    pub check: &'static str,
    pub message: String,
    pub fix: String,
}

/// Result of one diagnosis run.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Outcome {
    /// Highest tier fully passed (`None` = not even offline).
    pub reached: Option<Tier>,
    pub blocker: Option<Blocker>,
    /// Human-readable per-check lines (`✓`/`✗`).
    pub lines: Vec<String>,
}

impl Outcome {
    fn new() -> Self {
        Outcome {
            reached: None,
            blocker: None,
            lines: Vec::new(),
        }
    }

    fn pass(&mut self, line: String) {
        self.lines.push(format!("  ✓ {line}"));
    }

    fn fail(&mut self, tier: Tier, check: &'static str, message: String, fix: String) {
        self.lines.push(format!("  ✗ {check}: {message}"));
        self.blocker = Some(Blocker {
            tier,
            check,
            message,
            fix,
        });
    }
}

/// Env var names per prefix, in resolution order, for "what is missing".
fn missing_vars(env: &dyn Fn(&str) -> Option<String>) -> Vec<String> {
    let set = |name: &str| env(name).is_some_and(|v| !v.trim().is_empty());
    // The prefix with the most pieces set is the one the user is working
    // on; list what it still lacks. Nothing set → the canonical TOLE_*.
    let count = |p: &str| {
        ["BASE_URL", "MODEL", "API_KEY"]
            .iter()
            .filter(|s| set(&format!("{p}_{s}")))
            .count()
    };
    // First prefix wins ties, so TOLE_* (canonical) is the default.
    let best = ENV_PREFIXES
        .iter()
        .copied()
        .fold(("TOLE", 0usize), |acc, p| {
            let c = count(p);
            if c > acc.1 {
                (p, c)
            } else {
                acc
            }
        })
        .0;
    ["BASE_URL", "MODEL", "API_KEY"]
        .iter()
        .map(|s| format!("{best}_{s}"))
        .filter(|n| !set(n))
        .collect()
}

/// Cheap shape check: `http(s)://<host>[...]`, no whitespace.
fn base_url_ok(url: &str) -> bool {
    let rest = url
        .strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"));
    match rest {
        Some(r) => !r.is_empty() && !r.starts_with('/') && !url.chars().any(char::is_whitespace),
        None => false,
    }
}

/// `scheme://host[:port]/path` only: userinfo, query and fragment can carry
/// credentials and never belong in output or on disk.
pub fn redact_url(url: &str) -> String {
    let (scheme, rest) = match url.split_once("://") {
        Some((s, r)) => (format!("{s}://"), r),
        None => (String::new(), url),
    };
    let rest = rest.split(['?', '#']).next().unwrap_or("");
    let (authority, path) = match rest.find('/') {
        Some(i) => rest.split_at(i),
        None => (rest, ""),
    };
    let host = authority.rsplit('@').next().unwrap_or("");
    format!("{scheme}{host}{path}")
}

/// Strip the API key and any URL credentials from a diagnostic string.
fn sanitize(text: &str, cfg: &OpenAiConfig) -> String {
    // URL first: replacing the key first would break the URL match.
    let mut t = text.to_string();
    let shown = redact_url(&cfg.base_url);
    if shown != cfg.base_url {
        t = t.replace(&cfg.base_url, &shown);
    }
    if !cfg.api_key.is_empty() {
        t = t.replace(&cfg.api_key, "<redacted>");
    }
    t
}

/// Run the diagnosis up to `requested`, stopping at the first blocker.
/// `cfg` is the resolved provider (None = incomplete); `env` is only used
/// to name what is missing.
pub fn diagnose(
    cfg: Option<&OpenAiConfig>,
    env: &dyn Fn(&str) -> Option<String>,
    requested: Tier,
) -> Outcome {
    let mut out = diagnose_raw(cfg, env, requested);
    if let Some(cfg) = cfg {
        for l in &mut out.lines {
            *l = sanitize(l, cfg);
        }
        if let Some(b) = &mut out.blocker {
            b.message = sanitize(&b.message, cfg);
            b.fix = sanitize(&b.fix, cfg);
        }
    }
    out
}

fn diagnose_raw(
    cfg: Option<&OpenAiConfig>,
    env: &dyn Fn(&str) -> Option<String>,
    requested: Tier,
) -> Outcome {
    let mut out = Outcome::new();

    // ---- offline ----
    let Some(cfg) = cfg else {
        let missing = missing_vars(env);
        let fix = format!(
            "export {}",
            missing
                .iter()
                .map(|n| format!("{n}=<value>"))
                .collect::<Vec<_>>()
                .join(" ")
        );
        out.fail(
            Tier::Offline,
            "config",
            format!(
                "provider is not configured (missing: {})",
                missing.join(", ")
            ),
            fix,
        );
        return out;
    };
    if !base_url_ok(&cfg.base_url) {
        out.fail(
            Tier::Offline,
            "base_url",
            format!("base URL {:?} is not an http(s) URL", cfg.base_url),
            "export TOLE_BASE_URL=https://<host>/v1".into(),
        );
        return out;
    }
    out.pass(format!("offline: {} @ {}", cfg.model, cfg.base_url));
    out.reached = Some(Tier::Offline);
    if requested == Tier::Offline {
        return out;
    }

    // ---- catalog ----
    match fetch_model_ids(&cfg.base_url, &cfg.api_key) {
        Err(e) => {
            out.fail(
                Tier::Catalog,
                "models",
                e,
                format!(
                    "curl -sS -H \"Authorization: Bearer $TOLE_API_KEY\" {}/models",
                    cfg.base_url.trim_end_matches('/')
                ),
            );
            return out;
        }
        Ok(ids) if ids.is_empty() => {
            out.pass("catalog: provider lists no models (model check skipped)".into());
        }
        Ok(ids) if ids.iter().any(|i| i == &cfg.model) => {
            out.pass(format!(
                "catalog: {} is served ({} models)",
                cfg.model,
                ids.len()
            ));
        }
        Ok(ids) => {
            let some: Vec<&str> = ids.iter().take(5).map(String::as_str).collect();
            out.fail(
                Tier::Catalog,
                "model",
                format!(
                    "{:?} is not served ({} models listed)",
                    cfg.model,
                    ids.len()
                ),
                format!("export TOLE_MODEL=<one of: {}>", some.join(", ")),
            );
            return out;
        }
    }
    out.reached = Some(Tier::Catalog);
    out
}

/// CLI entry: diagnose and print. Returns `true` when the requested tier
/// was reached.
pub fn run_cli(
    cfg: Option<&OpenAiConfig>,
    env: &dyn Fn(&str) -> Option<String>,
    requested: Tier,
) -> bool {
    let outcome = diagnose(cfg, env, requested);
    println!("tole doctor — tier {}", requested.as_str());
    for l in &outcome.lines {
        println!("{l}");
    }
    if let Some(b) = &outcome.blocker {
        println!("blocked at {}: {}", b.tier.as_str(), b.message);
        println!("fix: {}", b.fix);
    }
    outcome.reached >= Some(requested) && outcome.blocker.is_none()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{json, Value};
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::sync::Arc;
    use std::time::Duration;

    fn no_env(_: &str) -> Option<String> {
        None
    }

    /// Scripted OpenAI-compatible server: `handler(method, path, body)`
    /// returns `(status, content_type, body)`.
    type Handler = dyn Fn(&str, &str, &Value) -> (u16, &'static str, String) + Send + Sync;

    fn serve(handler: Arc<Handler>) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            for conn in listener.incoming() {
                let Ok(mut s) = conn else { continue };
                let h = Arc::clone(&handler);
                std::thread::spawn(move || {
                    let _ = s.set_read_timeout(Some(Duration::from_secs(10)));
                    let mut buf = Vec::new();
                    let mut chunk = [0u8; 8192];
                    let (head, body) = loop {
                        if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                            let head = String::from_utf8_lossy(&buf[..i]).to_string();
                            let clen = head
                                .lines()
                                .find_map(|l| {
                                    let (k, v) = l.split_once(':')?;
                                    k.eq_ignore_ascii_case("content-length")
                                        .then(|| v.trim().parse::<usize>().ok())
                                        .flatten()
                                })
                                .unwrap_or(0);
                            if buf.len() >= i + 4 + clen {
                                break (head, buf[i + 4..i + 4 + clen].to_vec());
                            }
                        }
                        match s.read(&mut chunk) {
                            Ok(0) | Err(_) => return,
                            Ok(n) => buf.extend_from_slice(&chunk[..n]),
                        }
                    };
                    let mut first = head.lines().next().unwrap_or("").split_whitespace();
                    let (method, path) = (first.next().unwrap_or(""), first.next().unwrap_or(""));
                    let json: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
                    let (status, ctype, payload) = h(method, path, &json);
                    let resp = format!(
                        "HTTP/1.1 {status} X\r\nContent-Type: {ctype}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{payload}",
                        payload.len()
                    );
                    let _ = s.write_all(resp.as_bytes());
                });
            }
        });
        format!("http://{addr}/v1")
    }

    /// A provider that serves `GET /models` with the given ids.
    fn models_provider(models: Vec<&'static str>) -> String {
        serve(Arc::new(move |method, path, _| {
            if method == "GET" && path.ends_with("/models") {
                let data: Vec<Value> = models.iter().map(|m| json!({"id": m})).collect();
                (200, "application/json", json!({"data": data}).to_string())
            } else {
                (404, "application/json", "{}".to_string())
            }
        }))
    }

    fn cfg_for(url: String, model: &str) -> OpenAiConfig {
        OpenAiConfig::new(url, model, "sk-test")
    }

    #[test]
    fn offline_names_what_is_missing() {
        let env = |k: &str| (k == "TOLE_API_KEY").then(|| "sk".to_string());
        let o = diagnose(None, &env, Tier::Offline);
        let b = o.blocker.unwrap();
        assert_eq!(b.tier, Tier::Offline);
        assert!(b.message.contains("TOLE_BASE_URL") && b.message.contains("TOLE_MODEL"));
        assert!(!b.message.contains("TOLE_API_KEY"), "{}", b.message);
        assert!(
            b.fix.starts_with("export TOLE_BASE_URL=<value>"),
            "{}",
            b.fix
        );
        assert_eq!(o.reached, None);
    }

    #[test]
    fn offline_defaults_to_the_canonical_prefix() {
        let o = diagnose(None, &no_env, Tier::Offline);
        let b = o.blocker.unwrap();
        assert!(
            b.fix.starts_with(
                "export TOLE_BASE_URL=<value> TOLE_MODEL=<value> TOLE_API_KEY=<value>"
            ),
            "{}",
            b.fix
        );
        // A half-configured OPENAI_* set is the one to complete.
        let env = |k: &str| (k == "OPENAI_API_KEY").then(|| "sk".to_string());
        let b = diagnose(None, &env, Tier::Offline).blocker.unwrap();
        assert!(
            b.message.contains("OPENAI_BASE_URL") && !b.message.contains("TOLE_"),
            "{}",
            b.message
        );
    }

    #[test]
    fn offline_rejects_a_non_http_base_url() {
        let cfg = cfg_for("ftp://x".into(), "m");
        let o = diagnose(Some(&cfg), &no_env, Tier::Offline);
        assert_eq!(o.blocker.unwrap().check, "base_url");
        let ok = cfg_for("https://api.example.com/v1".into(), "m");
        let o = diagnose(Some(&ok), &no_env, Tier::Offline);
        assert_eq!(o.reached, Some(Tier::Offline));
        assert!(o.blocker.is_none());
    }

    #[test]
    fn catalog_flags_an_unserved_model() {
        let url = models_provider(vec!["alpha", "beta"]);
        let o = diagnose(Some(&cfg_for(url, "gamma")), &no_env, Tier::Catalog);
        let b = o.blocker.unwrap();
        assert_eq!((b.tier, b.check), (Tier::Catalog, "model"));
        assert!(b.fix.contains("alpha, beta"), "{}", b.fix);
        assert_eq!(o.reached, Some(Tier::Offline));
    }

    #[test]
    fn catalog_flags_an_unreachable_provider() {
        // Nothing listens on this port.
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = l.local_addr().unwrap().port();
        drop(l);
        let o = diagnose(
            Some(&cfg_for(format!("http://127.0.0.1:{port}/v1"), "m")),
            &no_env,
            Tier::Catalog,
        );
        let b = o.blocker.unwrap();
        assert_eq!(b.check, "models");
        assert!(b.fix.contains("/models"), "{}", b.fix);
        assert!(!b.message.contains("sk-test"));
    }

    #[test]
    fn urls_are_reduced_to_scheme_host_path() {
        assert_eq!(
            redact_url("https://u:p@host:8443/v1?key=abc#frag"),
            "https://host:8443/v1"
        );
        assert_eq!(redact_url("http://host"), "http://host");
        assert_eq!(redact_url("https://a@b@host/v1"), "https://host/v1");
    }

    #[test]
    fn diagnostics_never_carry_the_key_or_url_credentials() {
        let cfg = OpenAiConfig::new("ftp://user:hunter2@x/v1?k=sk-secret", "m", "sk-secret");
        let o = diagnose(Some(&cfg), &no_env, Tier::Offline);
        let all = format!("{:?}", o);
        assert!(
            !all.contains("hunter2") && !all.contains("sk-secret"),
            "{all}"
        );
        assert!(all.contains("ftp://x/v1"), "{all}");
    }

    #[test]
    fn tier_parse_roundtrip() {
        for t in [Tier::Offline, Tier::Catalog] {
            assert_eq!(Tier::parse(t.as_str()), Some(t));
        }
        assert_eq!(Tier::parse("nope"), None);
        assert_eq!(
            Tier::parse("full"),
            None,
            "the full tier is not available yet"
        );
        assert!(Tier::Offline < Tier::Catalog);
    }
}
