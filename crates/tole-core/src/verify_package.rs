//! Package-name existence verification (issue #144) — the slopsquatting
//! defense.
//!
//! LLMs hallucinate package names; attackers pre-register those names with
//! malware; agents (or humans) install them. Research numbers that shape
//! this tool: 43% of hallucinated names recur across runs (detectable),
//! 13% differ by one character from a real package (typo-squat bait).
//!
//! `verify_package` is ReadOnly: it never installs, it only asks the
//! registry "does this exist?" — making the SAFE path the CHEAP path
//! (no approval gate), while the actual `cargo add` / `bun add` still
//! flows through the normal Write gate.
//!
//! Registries (live-verified 2026-10-01):
//! - crates.io: `GET /api/v1/crates/{name}` (200 exists / 404 not),
//!   `GET /api/v1/crates?q={q}` for candidates
//! - npm: `GET https://registry.npmjs.org/{name}` (200/404),
//!   `GET /-/v1/search?text={q}` for candidates
//!
//! Honesty contract: 429/5xx → `status: "rate_limited"` — an UNKNOWN is
//! never reported as "does not exist".
//!
//! Testability: `ecosystems()` returns the request URLs; unit tests point
//! `base_override` at a local mock server instead of the real registries.

use crate::tool::{Risk, Tool};
use serde_json::{json, Value};
use std::time::Duration;

/// Default registry bases (overridable in tests).
const CRATES_BASE: &str = "https://crates.io/api/v1";
const NPM_BASE: &str = "https://registry.npmjs.org";
const HTTP_TIMEOUT: Duration = Duration::from_secs(15);

/// Validate a package name before it may appear in a URL path segment.
/// Allowlist charset; reject leading `-` (flag injection), `..` (path
/// games), and empty strings. Scoped npm names (`@scope/pkg`) are legal —
/// exactly one `/` and a leading `@`.
fn validate_name(name: &str) -> Result<(), String> {
    if name.is_empty() {
        return Err("name must not be empty".into());
    }
    if name.starts_with('-') {
        return Err("name must not start with '-'".into());
    }
    if name.contains("..") {
        return Err("name must not contain '..'".into());
    }
    let ok = name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-' | '@' | '/'));
    if !ok {
        return Err("name may only contain [a-zA-Z0-9._@/-] (registry package names)".into());
    }
    if name.starts_with('@') {
        // scoped npm: exactly one '/', non-empty scope and pkg
        let parts: Vec<&str> = name.split('/').collect();
        if parts.len() != 2 || parts[0].len() < 2 || parts[1].is_empty() {
            return Err("scoped names must look like @scope/pkg".into());
        }
    } else if name.contains('/') {
        return Err("'/' is only legal in scoped @scope/pkg names".into());
    }
    Ok(())
}

/// URL-encode a validated name per path segment (scoped names keep `/`).
fn url_encode(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    for (i, c) in name.char_indices() {
        if c == '/' {
            out.push('/');
        } else if c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-') {
            out.push(c);
        } else if c == '@' && i == 0 {
            out.push('@');
        } else {
            out.push_str(&format!("%{:02X}", c as u32));
        }
    }
    out
}

/// The comparable form of a package name for typo analysis: the bare
/// name (last `/` segment). A scoped query and a bare candidate must be
/// compared like-for-like — comparing `scope/foo` against the bare
/// fallback candidate `foo` verbatim is distance 6 and made every bare
/// fallback invisible to the typosquat warning (issue #235).
fn comparable_name(n: &str) -> &str {
    n.split('/').next_back().unwrap_or(n)
}

/// Levenshtein distance, capped at 2 (callers only care about ≤1).
fn edit_distance_capped(a: &str, b: &str) -> usize {
    if a == b {
        return 0;
    }
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    // Length diff > 1 ALREADY guarantees distance > 1 — decided before
    // the DP (issue #235: the old row-min early exit was subtle,
    // untested on adversarial inputs, and unnecessary at package-name
    // sizes; the length guard is provably sound on its own).
    if a.len().abs_diff(b.len()) > 1 {
        return 2;
    }
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    let mut cur = vec![0usize; b.len() + 1];
    for i in 1..=a.len() {
        cur[0] = i;
        for j in 1..=b.len() {
            let cost = usize::from(a[i - 1] != b[j - 1]);
            cur[j] = (prev[j] + 1).min(cur[j - 1] + 1).min(prev[j - 1] + cost);
        }
        std::mem::swap(&mut prev, &mut cur);
    }
    prev[b.len()].min(2)
}

/// The registry endpoints this tool queries (exposed for tests + docs).
fn endpoints(
    ecosystem: &str,
    name: &str,
    bases: Option<(&str, &str)>,
) -> Result<(String, String), String> {
    let (crates_base, npm_base) = bases.unwrap_or((CRATES_BASE, NPM_BASE));
    match ecosystem {
        "crates" => {
            validate_name(name)?;
            let enc = url_encode(name);
            Ok((
                format!("{crates_base}/crates/{enc}"),
                format!("{crates_base}/crates?q={enc}&per_page=5"),
            ))
        }
        "npm" => {
            validate_name(name)?;
            let enc = url_encode(name);
            Ok((
                format!("{npm_base}/{enc}"),
                format!("{npm_base}/-/v1/search?text={enc}&size=5"),
            ))
        }
        other => Err(format!(
            "unsupported ecosystem {other:?} — use \"crates\" or \"npm\""
        )),
    }
}

/// One GET returning `(status, body_json_or_null)`. Errors surface as a
/// rate-limited/unknown verdict, never as "does not exist".
fn fetch(agent: &ureq::Agent, url: &str) -> Result<(u16, Option<Value>), String> {
    // crates.io policy requires an identifying User-Agent; a named UA
    // also keeps anonymous throttling materially more forgiving.
    let resp = match agent
        .get(url)
        .header("User-Agent", "tole-verify_package (codecoradev/tole)")
        .call()
    {
        Ok(r) => r,
        // ureq 3: 4xx/5xx arrive as Err(StatusCode(code)) — the response
        // IS the answer here (exists/not-found/rate-limited are verdicts),
        // so unwrap it instead of treating it as a transport failure.
        Err(ureq::Error::StatusCode(code)) => {
            return Ok((code, None));
        }
        Err(e) => return Err(e.to_string()),
    };
    let status = resp.status().as_u16();
    if status == 204 {
        return Ok((status, None));
    }
    let mut resp = resp;
    let body = resp
        .body_mut()
        .read_json::<Value>()
        .map_err(|e| e.to_string())?;
    Ok((status, Some(body)))
}

struct RegistryCheck {
    exists: Option<bool>,
    newest_version: Option<String>,
    rate_limited: bool,
    candidates: Vec<String>,
}

fn check_registry(
    agent: &ureq::Agent,
    ecosystem: &str,
    name: &str,
    bases: Option<(&str, &str)>,
    want_candidates: bool,
) -> Result<RegistryCheck, String> {
    let (exists_url, search_url) = endpoints(ecosystem, name, bases)?;
    let mut out = RegistryCheck {
        exists: None,
        newest_version: None,
        rate_limited: false,
        candidates: Vec::new(),
    };

    match fetch(agent, &exists_url) {
        Ok((200, Some(body))) => {
            out.exists = Some(true);
            out.newest_version = body
                .get("crate")
                .and_then(|c| {
                    c.get("max_stable_version")
                        .or_else(|| c.get("newest_version"))
                })
                .and_then(Value::as_str)
                .map(str::to_string)
                .or_else(|| {
                    // npm shape: dist-tags.latest
                    body.get("dist-tags")
                        .and_then(|d| d.get("latest"))
                        .and_then(Value::as_str)
                        .map(str::to_string)
                });
        }
        Ok((404, _)) => out.exists = Some(false),
        Ok((s, _)) if s == 429 || s >= 500 => out.rate_limited = true,
        Ok((_, _)) => out.rate_limited = true, // unknown status: honest unknown
        Err(_) => out.rate_limited = true,     // network/parse error: honest unknown
    }

    // Candidates (issue #144, cora CI): fetched when the caller wants
    // them — exact=false ALWAYS (even when the name exists), and on a
    // not-found regardless of exact (nearest-real-name evidence is the
    // whole point there). exact=true + exists skips the search entirely.
    let need_candidates = want_candidates || out.exists == Some(false);
    if need_candidates {
        if let Ok((_, Some(body))) = fetch(agent, &search_url) {
            out.candidates = candidates_from(ecosystem, &body);
        }
    }
    // npm scoped 404: also try the bare name (foo of @scope/foo) on the
    // SAME base as the main lookup (tests override it; production = npm).
    if ecosystem == "npm" && name.starts_with('@') && out.exists == Some(false) {
        let npm_base = bases.map(|(_, n)| n).unwrap_or(NPM_BASE);
        let bare = name.split('/').next_back().unwrap_or(name);
        if let Ok((200, Some(body))) = fetch(agent, &format!("{npm_base}/{bare}")) {
            if body.get("name").and_then(Value::as_str) == Some(bare) {
                out.candidates.push(bare.to_string());
            }
        }
    }
    Ok(out)
}

fn candidates_from(ecosystem: &str, body: &Value) -> Vec<String> {
    match ecosystem {
        "crates" => body
            .get("crates")
            .and_then(Value::as_array)
            .map(|arr| {
                arr.iter()
                    .filter_map(|c| c.get("name").and_then(Value::as_str))
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default(),
        _ => body
            .get("objects")
            .and_then(Value::as_array)
            .map(|arr| {
                arr.iter()
                    .filter_map(|o| o.pointer("/package/name").and_then(Value::as_str))
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default(),
    }
}

/// Run the full verification. `base_override` (tests) replaces the
/// registry base so unit tests run against a local mock.
pub fn verify(
    ecosystem: &str,
    name: &str,
    exact: bool,
    base_override: Option<(&str, &str)>,
) -> Result<Value, String> {
    let agent = ureq::Agent::config_builder()
        .timeout_global(Some(HTTP_TIMEOUT))
        .build()
        .new_agent();
    let check = check_registry(&agent, ecosystem, name, base_override, !exact)?;

    let mut out = json!({
        "ecosystem": ecosystem,
        "name": name,
    });

    if check.rate_limited && check.exists.is_none() {
        out["status"] = json!("rate_limited");
        out["verdict"] =
            json!("unknown — registry unreachable or throttled; DO NOT treat as nonexistent");
        return Ok(out);
    }

    out["exists"] = json!(check.exists);
    if let Some(v) = &check.newest_version {
        out["newest_version"] = json!(v);
    }

    if check.exists == Some(false) || !exact {
        let near: Vec<&String> = check
            .candidates
            .iter()
            .filter(|c| edit_distance_capped(comparable_name(name), comparable_name(c)) <= 1)
            .collect();
        if !near.is_empty() {
            out["typo_squat_warning"] = json!(format!(
                "candidate(s) within edit distance 1: {} — confirm the exact name before installing",
                near.iter().map(|s| s.as_str()).collect::<Vec<_>>().join(", ")
            ));
            out["near_candidates"] = json!(near.iter().map(|s| s.as_str()).collect::<Vec<_>>());
        } else if !check.candidates.is_empty() {
            out["candidates"] = json!(check.candidates);
        }
    }

    out["verdict"] = match (check.exists, check.rate_limited) {
        (Some(true), _) => {
            json!("exists — safe to reference (still install through the normal approval gate)")
        }
        (Some(false), _) => json!(
            "NOT FOUND — likely a hallucinated package name; do NOT install; check candidates"
        ),
        _ => json!("unknown"),
    };
    Ok(out)
}

/// The ReadOnly tool surface.
#[derive(Debug, Clone, Default)]
pub struct VerifyPackageTool;

impl VerifyPackageTool {
    pub fn new() -> Self {
        Self
    }
}

impl Tool for VerifyPackageTool {
    fn name(&self) -> &str {
        "verify_package"
    }

    fn risk(&self) -> Risk {
        Risk::ReadOnly
    }

    fn describe(&self, input: &Value) -> String {
        let eco = input
            .get("ecosystem")
            .and_then(Value::as_str)
            .unwrap_or("?");
        let name = input.get("name").and_then(Value::as_str).unwrap_or("?");
        format!("verify {eco} package {name:?} exists in the registry")
    }

    fn spec(&self) -> Option<Value> {
        Some(json!({
            "type": "object",
            "properties": {
                "ecosystem": { "type": "string", "enum": ["crates", "npm"], "description": "Package registry to query" },
                "name": { "type": "string", "description": "Package name (npm scoped names like @scope/pkg are supported)" },
                "exact": { "type": "boolean", "description": "true (default): only the exact-name existence verdict; false: also include registry candidates" }
            },
            "required": ["ecosystem", "name"]
        }))
    }

    fn execute(&self, input: Value) -> Result<Value, String> {
        let eco = input
            .get("ecosystem")
            .and_then(Value::as_str)
            .ok_or_else(|| {
                "input must be {\"ecosystem\": \"crates\"|\"npm\", \"name\": \"...\"}".to_owned()
            })?;
        let name = input
            .get("name")
            .and_then(Value::as_str)
            .ok_or_else(|| "input must be {\"ecosystem\": ..., \"name\": \"...\"}".to_owned())?;
        let exact = input.get("exact").and_then(Value::as_bool).unwrap_or(true);
        // Validate up front so malformed names fail loudly (not as
        // "not found", which would read as a verdict).
        validate_name(name)?;
        verify(eco, name, exact, None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn name_validation_rejects_injection_shapes() {
        assert!(validate_name("serde").is_ok());
        assert!(validate_name("@scope/pkg").is_ok());
        assert!(validate_name("left-pad").is_ok());
        assert!(validate_name("").is_err());
        assert!(validate_name("-x").is_err());
        assert!(validate_name("..").is_err());
        assert!(validate_name("a/../b").is_err());
        assert!(validate_name("has space").is_err());
        assert!(validate_name("a/b").is_err(), "slash only legal scoped");
        assert!(validate_name("@s").is_err(), "unscoped @ is invalid");
    }

    #[test]
    fn url_encoding_is_segment_safe() {
        assert_eq!(url_encode("left-pad"), "left-pad");
        assert_eq!(url_encode("@scope/pkg"), "@scope/pkg");
        assert_eq!(url_encode("a~b"), "a%7Eb");
    }

    #[test]
    fn edit_distance_flags_single_char() {
        assert_eq!(edit_distance_capped("react", "react"), 0);
        assert_eq!(edit_distance_capped("reaat", "react"), 1);
        assert!(edit_distance_capped("completely-different", "react") >= 2);
    }

    /// Issue #235: boundary cases the old row-min early exit left
    /// untested. The behavior contract: distance values are EXACT up to
    /// the cap — equal length never false-caps, ±1 length is computed,
    /// >1 length is an immediate (provably sound) cap.
    #[test]
    fn edit_distance_boundary_cases() {
        // Multi-star / adversarial DP shapes: equal-length strings whose
        // every position differs. Values are EXACT up to the cap, 2 above.
        assert_eq!(edit_distance_capped("aaaa", "bbbb"), 2); // true 4, capped
        assert_eq!(edit_distance_capped("aaa", "aab"), 1);
        assert_eq!(edit_distance_capped("abc", "acb"), 2);
        assert_eq!(edit_distance_capped("kitten", "sitten"), 1);
        assert_eq!(edit_distance_capped("kitten", "sitting"), 2); // true 3, capped
                                                                  // Length ±1 boundaries: insertion/deletion cases.
        assert_eq!(edit_distance_capped("abc", "ab"), 1);
        assert_eq!(edit_distance_capped("ab", "abc"), 1);
        assert_eq!(edit_distance_capped("abc", "x"), 2); // true 2 (capped)
                                                         // Length diff > 1: cap BEFORE the DP — but the RESULT must equal
                                                         // the true capped distance (regression guard for the old
                                                         // combined guard ordering).
        assert_eq!(edit_distance_capped("abc", "x"), 2);
        assert_eq!(edit_distance_capped("abcdefgh", "xy"), 2);
        // Unicode: distance is over chars, not bytes.
        assert_eq!(edit_distance_capped("héllo", "hallo"), 1);
    }

    /// Issue #235: the typosquat comparison uses the BARE name — a
    /// scoped query `@scope/reaat` must flag the bare candidate `react`
    /// (distance 1), which the old `trim_start_matches('@')` comparison
    /// missed ("scope/reaat" vs "react" = far over the cap).
    #[test]
    fn comparable_name_bares_scoped_queries() {
        assert_eq!(comparable_name("@scope/react"), "react");
        assert_eq!(comparable_name("react"), "react");
        assert_eq!(comparable_name("@a/b/c"), "c");
    }

    #[test]
    fn endpoints_reject_bad_ecosystem() {
        assert!(endpoints("pypi", "requests", None).is_err());
        let (a, _) = endpoints("crates", "serde", None).unwrap();
        assert!(a.starts_with("https://crates.io/api/v1/crates/serde"));
        let (a2, _) = endpoints(
            "npm",
            "serde",
            Some(("http://127.0.0.1:1/c", "http://127.0.0.1:1/n")),
        )
        .unwrap();
        assert!(a2.starts_with("http://127.0.0.1:1/n/serde"));
    }

    #[test]
    fn spec_advertises_exactly_what_execute_reads() {
        let spec = VerifyPackageTool::new().spec().unwrap();
        let props = spec.get("properties").unwrap();
        for k in ["ecosystem", "name", "exact"] {
            assert!(props.get(k).is_some(), "spec missing {k}");
        }
        assert_eq!(props.as_object().unwrap().len(), 3, "spec overstates");
    }

    #[test]
    fn execute_rejects_malformed_names_loudly() {
        let t = VerifyPackageTool::new();
        let err = t
            .execute(json!({"ecosystem": "crates", "name": "-flag"}))
            .unwrap_err();
        assert!(
            err.contains("-"),
            "must fail loudly, not 'not found': {err}"
        );
    }

    #[test]
    fn risk_is_read_only() {
        assert_eq!(VerifyPackageTool::new().risk(), Risk::ReadOnly);
    }
}

#[cfg(test)]
mod mock_server_tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::sync::atomic::{AtomicU16, Ordering};

    static PORT: AtomicU16 = AtomicU16::new(0);

    fn start_mock(
        responder: impl Fn(&str) -> (u16, &'static str) + Send + Sync + 'static,
    ) -> (String, String) {
        let port = PORT.fetch_add(1, Ordering::SeqCst);
        let listener = TcpListener::bind(("127.0.0.1", 18000 + (port % 2000))).unwrap();
        let addr = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let mut s = match stream {
                    Ok(s) => s,
                    Err(_) => continue,
                };
                // Read until the end-of-headers marker (CRLF CRLF) —
                // one read() may return early and split the request.
                let mut buf = Vec::new();
                let mut chunk = [0u8; 1024];
                loop {
                    let n = match s.read(&mut chunk) {
                        Ok(0) | Err(_) => break,
                        Ok(n) => n,
                    };
                    buf.extend_from_slice(&chunk[..n]);
                    if buf.windows(4).any(|w| w == b"\r\n\r\n") || buf.len() > 8192 {
                        break;
                    }
                }
                let req = String::from_utf8_lossy(&buf).to_string();
                let path = req.split_whitespace().nth(1).unwrap_or("/").to_string();
                let (code, body) = responder(&path);
                let reason = if code == 200 {
                    "OK"
                } else if code == 404 {
                    "Not Found"
                } else {
                    "Too Many Requests"
                };
                let resp = format!(
                    "HTTP/1.1 {code} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = s.write_all(resp.as_bytes());
                let _ = s.flush();
            }
        });
        (format!("http://{addr}/c"), format!("http://{addr}/n"))
    }

    #[test]
    fn mock_crates_not_found_with_near_candidate_flags_typo_squat() {
        let bases = start_mock(|path| {
            if path.starts_with("/c/crates?q=tokyo") {
                (200, r#"{"crates":[{"name":"tokio"},{"name":"tokyod"}]}"#)
            } else if path == "/c/crates/tokyo" {
                (404, r#"{"errors":[{"detail":"not found"}]}"#)
            } else {
                (404, "{}")
            }
        });
        let out = verify("crates", "tokyo", true, Some((&bases.0, &bases.1))).unwrap();
        assert_eq!(out["exists"], json!(false));
        assert!(out["typo_squat_warning"]
            .as_str()
            .unwrap()
            .contains("tokio"));
        assert!(out["verdict"].as_str().unwrap().contains("NOT FOUND"));
    }

    #[test]
    fn mock_crates_429_is_honest_unknown() {
        let bases = start_mock(|_path| (429, r#"{"errors":"slow down"}"#));
        let out = verify("crates", "mystery", true, Some((&bases.0, &bases.1))).unwrap();
        assert_eq!(out["status"], json!("rate_limited"));
        assert!(
            out.get("exists").is_none(),
            "unknown must not claim exists:false"
        );
        assert!(out["verdict"].as_str().unwrap().contains("unknown"));
    }

    #[test]
    fn mock_npm_scoped_not_found_suggests_bare_name() {
        let bases = start_mock(|path| {
            if path == "/n/@leftscope/pad" {
                (404, "{}")
            } else if path.starts_with("/n/-/v1/search") {
                (200, r#"{"objects":[{"package":{"name":"pad-left"}}]}"#)
            } else if path == "/n/pad" {
                (200, r#"{"name":"pad","dist-tags":{"latest":"1.0.0"}}"#)
            } else {
                (404, "{}")
            }
        });
        let out = verify("npm", "@leftscope/pad", true, Some((&bases.0, &bases.1))).unwrap();
        assert_eq!(out["exists"], json!(false));
        // Issue #235: the bare fallback candidate "pad" is distance 0
        // from the query's bare name — it now surfaces as a TYPOSQUAT
        // WARNING (the old comparison never matched scoped-vs-bare and
        // dropped it into plain `candidates`).
        assert!(
            out["typo_squat_warning"].as_str().unwrap().contains("pad"),
            "{out}"
        );
        assert!(out["near_candidates"].as_array().is_some());
    }

    #[test]
    fn mock_exact_false_on_existing_name_returns_candidates() {
        // cora CI: exact=false must NOT be a silent no-op — candidates are
        // fetched even when the name exists (adjacent-name discovery).
        let bases = start_mock(|path| {
            if path == "/c/crates/serde" {
                (
                    200,
                    r#"{"crate":{"name":"serde","max_stable_version":"1.0.229"}}"#,
                )
            } else if path.starts_with("/c/crates?q=serde") {
                (
                    200,
                    r#"{"crates":[{"name":"serde_json"},{"name":"serde_yaml"}]}"#,
                )
            } else {
                (404, "{}")
            }
        });
        let out = verify("crates", "serde", false, Some((&bases.0, &bases.1))).unwrap();
        assert_eq!(out["exists"], json!(true));
        let cands = out["candidates"].as_array().expect("candidates present");
        assert!(cands.iter().any(|c| c == "serde_json"));
        // exact=true on the same name never searches: no candidates key.
        let out2 = verify("crates", "serde", true, Some((&bases.0, &bases.1))).unwrap();
        assert!(out2.get("candidates").is_none());
    }

    #[test]
    fn mock_exists_reports_version() {
        let bases = start_mock(|path| {
            if path == "/c/crates/serde" {
                (
                    200,
                    r#"{"crate":{"name":"serde","max_stable_version":"1.0.229"}}"#,
                )
            } else {
                (404, "{}")
            }
        });
        let out = verify("crates", "serde", true, Some((&bases.0, &bases.1))).unwrap();
        assert_eq!(out["exists"], json!(true));
        assert_eq!(out["newest_version"], json!("1.0.229"));
    }
}
