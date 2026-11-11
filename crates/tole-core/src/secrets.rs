//! Known-value secret redaction (#595, design `secrets-integrations-design.md`
//! §4): one place that knows which secret VALUES this process holds, and one
//! redactor every egress sink calls before text leaves the process.
//!
//! - [`is_secret_name`] is the single name rule: the child-process env scrub
//!   (ENV-1, `subprocess::scrub_env_for_child`) and [`SecretSet::collect`]
//!   both use it, so "secret-shaped" cannot mean two things.
//! - [`SecretSet::collect`] takes the values of secret-named env vars plus
//!   values handed in explicitly (the provider key, `${NAME}` header
//!   expansions, URL userinfo passwords). A value shorter than
//!   [`MIN_SECRET_LEN`] characters is NOT collected (it would redact ordinary
//!   words); its NAME is reported through [`SecretSet::skipped`] so a
//!   `doctor` can warn. No report ever carries a value.
//! - [`SecretSet::redact`] replaces every known value, and its base64
//!   (standard and URL-safe), percent-encoded and JSON-escaped forms, with
//!   `«NAME»`. Matches that overlap are merged into one redacted span, so a
//!   secret overlapping another never leaks a tail. Text without a match,
//!   and every text when the set is empty, is returned borrowed (no copy).
//!
//! Matching is a first-byte-indexed scan with needles tried longest first:
//! no regex or automaton dependency. A set holds a handful of secrets, each
//! with up to six forms, so the per-byte work is a table lookup plus, for the
//! few bytes that start a needle, a short prefix compare.
//!
//! Platform-agnostic: pure std + `base64`, no I/O beyond [`SecretSet::from_env`]
//! reading the process environment.

use base64::Engine;
use std::borrow::Cow;
use std::sync::Arc;

/// Values with fewer characters than this are not collected: an 8-char
/// floor keeps `true`, `1`, `admin` and similar from redacting ordinary text.
pub const MIN_SECRET_LEN: usize = 8;

/// Name substrings that make an env var secret-shaped (case-insensitive):
/// `SECRET_KEY`, `AUTHTOKEN`, `DB_PASSWD` all match (#284).
const SECRET_NAME_MARKERS: [&str; 8] = [
    "API_KEY",
    "APIKEY",
    "SECRET",
    "TOKEN",
    "PASSWORD",
    "PASSWD",
    "CREDENTIAL",
    "PRIVATE_KEY",
];

/// Names the markers sweep up that are not secrets: a non-secret HF tuning
/// knob that the broad `TOKEN` match would otherwise catch.
const NOT_SECRET: [&str; 1] = ["TOKENIZERS_PARALLELISM"];

/// Whether an env var NAME is secret-shaped. The single name rule for the
/// child-env scrub and for [`SecretSet::collect`].
pub fn is_secret_name(name: &str) -> bool {
    let upper = name.to_ascii_uppercase();
    SECRET_NAME_MARKERS.iter().any(|m| upper.contains(m)) && !NOT_SECRET.contains(&upper.as_str())
}

/// Why a named value was not collected. Carries no value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SkipReason {
    /// Fewer than [`MIN_SECRET_LEN`] characters.
    TooShort,
    /// The value occurs inside a `«NAME»` marker; redacting it would break
    /// idempotence (pathological: a value equal to a secret's name).
    InsideMarker,
}

/// A secret that is known by name but not redacted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Skipped {
    pub name: String,
    pub reason: SkipReason,
}

/// The known secret values of this process. Cheap to clone (shared).
/// `Default` is the empty set, for which [`SecretSet::redact`] is a no-op.
#[derive(Clone, Default)]
pub struct SecretSet {
    inner: Arc<Inner>,
}

#[derive(Default)]
struct Inner {
    /// Collected `(name, value)` in precedence order, deduped by value.
    entries: Vec<(String, String)>,
    skipped: Vec<Skipped>,
    /// `«NAME»` per entry.
    markers: Vec<String>,
    /// Every form of every value, longest first.
    needles: Vec<Needle>,
    /// Needle indices by first byte (256 slots; empty when no needles).
    by_first: Vec<Vec<u32>>,
}

struct Needle {
    bytes: Box<[u8]>,
    entry: usize,
}

impl std::fmt::Debug for SecretSet {
    /// Names and counts only: a `{:?}` of a set must never print a value.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SecretSet")
            .field("names", &self.names())
            .field("skipped", &self.inner.skipped)
            .finish()
    }
}

impl SecretSet {
    /// Collect from an environment snapshot (only secret-shaped names count)
    /// plus explicitly named values (header `${NAME}` expansions, URL
    /// passwords, a provider key passed in code). Values are trimmed; empty
    /// values are ignored; when two names hold the same value the first
    /// wins (env entries sorted by name, then the explicit ones in order).
    pub fn collect<E, X>(env: E, explicit: X) -> Self
    where
        E: IntoIterator<Item = (String, String)>,
        X: IntoIterator<Item = (String, String)>,
    {
        let mut from_env: Vec<(String, String)> =
            env.into_iter().filter(|(k, _)| is_secret_name(k)).collect();
        from_env.sort();
        Self::build(from_env.into_iter().chain(explicit))
    }

    /// [`SecretSet::collect`] over the process environment (non-UTF-8
    /// entries are skipped: they cannot appear in `&str` text anyway).
    pub fn from_env() -> Self {
        let env = std::env::vars_os()
            .filter_map(|(k, v)| Some((k.into_string().ok()?, v.into_string().ok()?)));
        Self::collect(env, std::iter::empty())
    }

    /// This set plus one explicitly named value. A value already known
    /// keeps its existing name.
    pub fn with(&self, name: &str, value: &str) -> Self {
        let mut all = self.inner.entries.clone();
        all.push((name.to_string(), value.to_string()));
        let mut set = Self::build(all);
        let inner = Arc::get_mut(&mut set.inner).expect("freshly built set is unshared");
        let mut skipped = self.inner.skipped.clone();
        skipped.append(&mut inner.skipped);
        inner.skipped = skipped;
        set
    }

    /// This set plus the userinfo password of `url` (`scheme://user:pw@host`),
    /// as it appears in the url, under `name`. A url without one is a no-op.
    pub fn with_url_password(&self, name: &str, url: &str) -> Self {
        match url_password(url) {
            Some(pw) => self.with(name, pw),
            None => self.clone(),
        }
    }

    /// The names of the collected secrets (never values).
    pub fn names(&self) -> Vec<&str> {
        self.inner.entries.iter().map(|(n, _)| n.as_str()).collect()
    }

    /// Secrets known by name but not redacted, with the reason.
    pub fn skipped(&self) -> &[Skipped] {
        &self.inner.skipped
    }

    /// True when nothing would ever be redacted.
    pub fn is_empty(&self) -> bool {
        self.inner.needles.is_empty()
    }

    /// Replace every known value (and its encoded forms) with `«NAME»`.
    /// Overlapping matches merge into one span carrying each name once.
    /// Idempotent; never splits a UTF-8 character (every needle is valid
    /// UTF-8, so a match starts and ends on character boundaries).
    pub fn redact<'a>(&self, text: &'a str) -> Cow<'a, str> {
        let inner = &*self.inner;
        if inner.needles.is_empty() {
            return Cow::Borrowed(text);
        }
        let hay = text.as_bytes();
        let mut out = String::new();
        let mut copied = 0usize;
        let mut span: Option<(usize, usize, Vec<usize>)> = None;
        for i in 0..hay.len() {
            let cands = &inner.by_first[hay[i] as usize];
            let Some(nd) = cands
                .iter()
                .map(|&n| &inner.needles[n as usize])
                .find(|nd| hay[i..].starts_with(&nd.bytes))
            else {
                continue;
            };
            let end = i + nd.bytes.len();
            match &mut span {
                Some((_, s_end, names)) if i < *s_end => {
                    *s_end = (*s_end).max(end);
                    if !names.contains(&nd.entry) {
                        names.push(nd.entry);
                    }
                }
                _ => {
                    if let Some(done) = span.take() {
                        copied = emit(&mut out, text, copied, done, &inner.markers);
                    }
                    span = Some((i, end, vec![nd.entry]));
                }
            }
        }
        let Some(done) = span else {
            return Cow::Borrowed(text);
        };
        copied = emit(&mut out, text, copied, done, &inner.markers);
        out.push_str(&text[copied..]);
        Cow::Owned(out)
    }

    /// [`SecretSet::redact`] over every string (object keys included) of a
    /// JSON value. Structure-preserving: a redaction can never break the
    /// JSON, which redacting its serialized text could (a value containing
    /// `","` would match across two fields).
    pub fn redact_value(&self, value: &serde_json::Value) -> serde_json::Value {
        use serde_json::Value;
        if self.is_empty() {
            return value.clone();
        }
        match value {
            Value::String(s) => Value::String(self.redact(s).into_owned()),
            Value::Array(a) => Value::Array(a.iter().map(|v| self.redact_value(v)).collect()),
            Value::Object(o) => Value::Object(
                o.iter()
                    .map(|(k, v)| (self.redact(k).into_owned(), self.redact_value(v)))
                    .collect(),
            ),
            // A secret of digits can be a bare JSON number; it becomes the
            // marker string, which keeps the JSON valid.
            Value::Number(n) => match self.redact(&n.to_string()) {
                Cow::Owned(r) => Value::String(r),
                Cow::Borrowed(_) => value.clone(),
            },
            other => other.clone(),
        }
    }

    fn build(entries: impl IntoIterator<Item = (String, String)>) -> Self {
        let mut inner = Inner::default();
        for (name, value) in entries {
            let value = value.trim();
            if value.is_empty() || inner.entries.iter().any(|(_, v)| v == value) {
                continue;
            }
            if value.chars().count() < MIN_SECRET_LEN {
                inner.skipped.push(Skipped {
                    name,
                    reason: SkipReason::TooShort,
                });
                continue;
            }
            inner.entries.push((name, value.to_string()));
        }
        inner.markers = inner
            .entries
            .iter()
            .map(|(n, _)| format!("«{n}»"))
            .collect();
        // A value inside a marker would re-match its own redaction.
        let markers = inner.markers.clone();
        let mut kept = Vec::new();
        for (name, value) in std::mem::take(&mut inner.entries) {
            if markers.iter().any(|m| m.contains(&value)) {
                inner.skipped.push(Skipped {
                    name,
                    reason: SkipReason::InsideMarker,
                });
            } else {
                kept.push((name, value));
            }
        }
        inner.entries = kept;
        inner.markers = inner
            .entries
            .iter()
            .map(|(n, _)| format!("«{n}»"))
            .collect();
        let mut needles: Vec<Needle> = Vec::new();
        for (idx, (_, value)) in inner.entries.iter().enumerate() {
            for form in forms(value) {
                if !needles.iter().any(|n| *n.bytes == *form.as_bytes()) {
                    needles.push(Needle {
                        bytes: form.into_bytes().into_boxed_slice(),
                        entry: idx,
                    });
                }
            }
        }
        // Stable: equal lengths keep precedence order.
        needles.sort_by_key(|n| std::cmp::Reverse(n.bytes.len()));
        if !needles.is_empty() {
            inner.by_first = vec![Vec::new(); 256];
            for (i, n) in needles.iter().enumerate() {
                inner.by_first[n.bytes[0] as usize].push(i as u32);
            }
        }
        inner.needles = needles;
        Self {
            inner: Arc::new(inner),
        }
    }
}

/// Write the text before a redacted span and the span's markers; returns
/// the new copy position (the span's end).
fn emit(
    out: &mut String,
    text: &str,
    copied: usize,
    (start, end, names): (usize, usize, Vec<usize>),
    markers: &[String],
) -> usize {
    out.push_str(&text[copied..start]);
    for n in names {
        out.push_str(&markers[n]);
    }
    end
}

/// The forms of a value that can reach a sink: as is, JSON-escaped (inside a
/// serialized JSON string), base64 standard and URL-safe (no padding: the
/// padded form contains it), and percent-encoded (upper- and lowercase hex).
fn forms(value: &str) -> Vec<String> {
    let mut v = vec![value.to_string()];
    if let Ok(json) = serde_json::to_string(value) {
        v.push(json[1..json.len() - 1].to_string());
    }
    v.push(base64::engine::general_purpose::STANDARD_NO_PAD.encode(value));
    v.push(base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(value));
    // Only the hex digits differ between the two encodings; literal
    // characters keep their case (lowercasing the whole string would miss
    // `Ab%2fcd` and match unrelated lowercase text).
    v.push(percent_encode(value, true));
    v.push(percent_encode(value, false));
    v.dedup();
    v
}

/// RFC 3986 percent-encoding of everything outside the unreserved set,
/// with upper- or lowercase hex digits.
fn percent_encode(value: &str, upper: bool) -> String {
    let mut out = String::with_capacity(value.len());
    for &b in value.as_bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~') {
            out.push(b as char);
        } else {
            out.push_str(&if upper {
                format!("%{b:02X}")
            } else {
                format!("%{b:02x}")
            });
        }
    }
    out
}

/// The userinfo password of `scheme://user:pw@host/...`, as written.
fn url_password(url: &str) -> Option<&str> {
    let rest = &url[url.find("://")? + 3..];
    let authority = &rest[..rest.find(['/', '?', '#']).unwrap_or(rest.len())];
    let userinfo = &authority[..authority.rfind('@')?];
    let pw = &userinfo[userinfo.find(':')? + 1..];
    (!pw.is_empty()).then_some(pw)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn set(pairs: &[(&str, &str)]) -> SecretSet {
        SecretSet::collect(
            pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())),
            std::iter::empty(),
        )
    }

    #[test]
    fn the_name_rule_matches_the_env1_shapes() {
        for n in [
            "TOLE_API_KEY",
            "openai_apikey",
            "SECRET_KEY",
            "TOLE_TEST_AUTHTOKEN",
            "DB_PASSWD",
            "MY_PASSWORD",
            "AWS_CREDENTIALS",
            "SSH_PRIVATE_KEY",
            "GITHUB_TOKEN",
        ] {
            assert!(is_secret_name(n), "{n}");
        }
        for n in ["PATH", "HOME", "TOLE_MODEL", "TOKENIZERS_PARALLELISM"] {
            assert!(!is_secret_name(n), "{n}");
        }
    }

    #[test]
    fn collect_takes_secret_named_env_and_explicit_values_only() {
        let s = SecretSet::collect(
            [
                ("TOLE_API_KEY".to_string(), "sk-live-0123456789".to_string()),
                ("HOME".to_string(), "/home/someone-long".to_string()),
            ],
            [("NGINJEN_KEY".to_string(), "ngx-abcdefghij".to_string())],
        );
        assert_eq!(s.names(), vec!["TOLE_API_KEY", "NGINJEN_KEY"]);
        assert_eq!(
            s.redact("a sk-live-0123456789 b ngx-abcdefghij c /home/someone-long"),
            "a «TOLE_API_KEY» b «NGINJEN_KEY» c /home/someone-long"
        );
    }

    #[test]
    fn short_values_are_reported_by_name_never_by_value() {
        let s = set(&[("SHORT_TOKEN", "abc1234"), ("LONG_TOKEN", "abcd12345")]);
        assert_eq!(
            s.skipped(),
            &[Skipped {
                name: "SHORT_TOKEN".into(),
                reason: SkipReason::TooShort
            }]
        );
        // The short value is left alone (false-positive guard) ...
        assert_eq!(s.redact("abc1234 abcd12345"), "abc1234 «LONG_TOKEN»");
        // ... and no report or debug output carries a value.
        let dbg = format!("{s:?}");
        assert!(
            !dbg.contains("abc1234") && !dbg.contains("abcd12345"),
            "{dbg}"
        );
        assert!(dbg.contains("SHORT_TOKEN") && dbg.contains("LONG_TOKEN"));
    }

    #[test]
    fn eight_characters_counted_as_chars_not_bytes() {
        // 7 chars but 14 bytes: still too short.
        let s = set(&[("U_TOKEN", "ééééééé")]);
        assert!(s.is_empty());
        assert_eq!(s.skipped()[0].reason, SkipReason::TooShort);
    }

    #[test]
    fn encoded_forms_are_redacted() {
        let v = "p@ss/w??rd>>>key=1";
        let s = set(&[("DB_PASSWORD", v)]);
        use base64::engine::general_purpose::{STANDARD, URL_SAFE};
        let std64 = STANDARD.encode(v);
        let url64 = URL_SAFE.encode(v);
        assert_ne!(std64, url64, "the value must exercise both alphabets");
        for enc in [
            std64,
            url64,
            "p%40ss%2Fw%3F%3Frd%3E%3E%3Ekey%3D1".to_string(),
            "p%40ss%2fw%3f%3frd%3e%3e%3ekey%3d1".to_string(),
        ] {
            let out = s
                .redact(&format!("Authorization: Basic {enc}"))
                .into_owned();
            assert!(out.contains("«DB_PASSWORD»"), "{enc} -> {out}");
            assert!(!out.contains(&enc[..enc.len().min(12)]), "{enc} -> {out}");
        }
    }

    #[test]
    fn percent_forms_keep_the_case_of_literal_characters() {
        // Mixed case plus a reserved character: only the hex digits of the
        // escape may change case between encoders.
        let s = set(&[("MIX_TOKEN", "Ab/cdEFGHIJ")]);
        for url in [
            "https://x.example/?k=Ab%2FcdEFGHIJ",
            "https://x.example/?k=Ab%2fcdEFGHIJ",
        ] {
            assert_eq!(s.redact(url), "https://x.example/?k=«MIX_TOKEN»", "{url}");
        }
    }

    #[test]
    fn the_lowercased_copy_of_a_mixed_case_secret_is_not_a_needle() {
        let s = set(&[("MIX_TOKEN", "AbCdEfGh1234")]);
        assert_eq!(s.redact("abcdefgh1234"), "abcdefgh1234");
        assert_eq!(s.redact("AbCdEfGh1234"), "«MIX_TOKEN»");
    }

    #[test]
    fn json_escaped_form_is_redacted_in_serialized_arguments() {
        let v = r#"ab"cd\efgh"#;
        let s = set(&[("X_SECRET", v)]);
        let args = serde_json::json!({ "content": format!("key={v}") }).to_string();
        assert_eq!(s.redact(&args), r#"{"content":"key=«X_SECRET»"}"#);
    }

    #[test]
    fn longest_secret_wins_over_its_prefix() {
        let s = set(&[("A_TOKEN", "abcdefgh"), ("B_TOKEN", "abcdefgh-longer")]);
        assert_eq!(s.redact("x abcdefgh-longer y"), "x «B_TOKEN» y");
        assert_eq!(s.redact("x abcdefgh y"), "x «A_TOKEN» y");
    }

    #[test]
    fn overlapping_secrets_merge_and_leak_no_tail() {
        let s = set(&[("A_TOKEN", "11112222xx"), ("B_TOKEN", "2222xx33334444")]);
        let out = s.redact("<11112222xx33334444>");
        assert_eq!(out, "<«A_TOKEN»«B_TOKEN»>");
        assert!(!out.contains("3333"));
    }

    #[test]
    fn redaction_is_idempotent() {
        let s = set(&[
            ("A_TOKEN", "abcdefgh"),
            ("B_TOKEN", "abcdefgh-longer"),
            ("C_SECRET", "ünïcødé-sécret"),
        ]);
        let text = "abcdefgh abcdefgh-longer ünïcødé-sécret YWJjZGVmZ2g done";
        let once = s.redact(text).into_owned();
        assert_eq!(s.redact(&once), once);
        assert!(!once.contains("abcdefgh") && !once.contains("YWJjZGVmZ2g"));
    }

    #[test]
    fn a_value_inside_a_marker_is_skipped_to_keep_idempotence() {
        // Pathological: a secret whose value is another secret's name.
        let s = set(&[("A_TOKEN", "B_TOKEN_X"), ("B_TOKEN_X", "0123456789")]);
        assert!(s
            .skipped()
            .iter()
            .any(|k| k.name == "A_TOKEN" && k.reason == SkipReason::InsideMarker));
        let once = s.redact("0123456789").into_owned();
        assert_eq!(once, "«B_TOKEN_X»");
        assert_eq!(s.redact(&once), once);
    }

    #[test]
    fn utf8_boundaries_and_unicode_values_survive() {
        let s = set(&[("U_SECRET", "日本語のひみつ鍵")]);
        let out = s.redact("前日本語のひみつ鍵後 and é日本語のひみつ鍵é");
        assert_eq!(out, "前«U_SECRET»後 and é«U_SECRET»é");
        // An ASCII secret next to multibyte text.
        let s = set(&[("A_TOKEN", "abcdefgh")]);
        assert_eq!(s.redact("éabcdefghé"), "é«A_TOKEN»é");
    }

    #[test]
    fn empty_set_is_a_borrowed_no_op() {
        let s = SecretSet::default();
        assert!(s.is_empty());
        assert!(matches!(s.redact("anything at all"), Cow::Borrowed(_)));
        let s = set(&[("A_TOKEN", "abcdefgh")]);
        assert!(
            matches!(s.redact("no match here"), Cow::Borrowed(_)),
            "a miss must not copy"
        );
    }

    #[test]
    fn with_adds_explicit_values_and_keeps_existing_names() {
        let s =
            set(&[("OPENAI_API_KEY", "sk-same-value-1")]).with("TOLE_API_KEY", "sk-same-value-1");
        assert_eq!(s.names(), vec!["OPENAI_API_KEY"]);
        let s = s.with("X", "short");
        assert_eq!(s.skipped()[0].name, "X");
        let s = s.with("H_KEY", "header-value-1");
        assert_eq!(s.redact("header-value-1"), "«H_KEY»");
        assert_eq!(s.skipped().len(), 1, "earlier reports are kept");
    }

    #[test]
    fn url_passwords_are_collected_as_written() {
        let s = SecretSet::default()
            .with_url_password("MCP_URL", "https://bob:hunter2-long@example.com/mcp?x=1");
        assert_eq!(
            s.redact("GET https://bob:hunter2-long@example.com/mcp"),
            "GET https://bob:«MCP_URL»@example.com/mcp"
        );
        for url in [
            "https://example.com/a@b",
            "https://bob@example.com/",
            "not a url",
            "https://bob:@example.com/",
        ] {
            assert!(
                SecretSet::default().with_url_password("U", url).is_empty(),
                "{url}"
            );
        }
    }

    #[test]
    fn redact_value_keeps_the_json_structure() {
        // A value that spans two fields if the serialized text were redacted.
        let s = set(&[("X_TOKEN", r#"a","b":"c"#)]);
        let v = serde_json::json!({"a": "x", "b": ["c", {"k a\",\"b\":\"c": r#"pre a","b":"c post"#}], "n": 1});
        let out = s.redact_value(&v);
        assert_eq!(
            out,
            serde_json::json!({"a": "x", "b": ["c", {"k «X_TOKEN»": "pre «X_TOKEN» post"}], "n": 1})
        );
        assert_eq!(SecretSet::default().redact_value(&v), v);
    }

    #[test]
    fn redact_value_turns_a_secret_number_into_the_marker_string() {
        let s = set(&[("PIN_SECRET", "12345678")]);
        let v = serde_json::json!({"pin": 12345678, "other": 87654321, "f": 1.5});
        assert_eq!(
            s.redact_value(&v),
            serde_json::json!({"pin": "«PIN_SECRET»", "other": 87654321, "f": 1.5})
        );
    }

    /// Throughput measurement, not a gate (no timing assertion): run with
    /// `cargo test -p tole-core --release secrets::tests::throughput -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn throughput() {
        let s = set(&[
            ("TOLE_API_KEY", "test-provider-key-a1B2c3D4e5F6g7H8"),
            ("TOLE_MCP_X_TOKEN", "mcp-tok-0123456789abcdef"),
            ("SYSTEMONE_API_KEY", "s1-ZZZZyyyyXXXXwwww"),
            ("NGINJEN_API_KEY", "ngx_live_5f3e2d1c"),
            ("GITHUB_TOKEN", "test-gh-token-abcdefghijklmnopqrstu"),
        ]);
        let unit = "fn main() { let s = \"some text, json {\\\"k\\\": 1}\"; println!(\"{s}\"); } // émoji ✓\n";
        for size in [4 * 1024, 64 * 1024, 1024 * 1024] {
            let text: String = unit.repeat(size / unit.len() + 1);
            for (label, set) in [("5 secrets", &s), ("empty", &SecretSet::default())] {
                let iters = (64 * 1024 * 1024 / text.len()).max(4);
                let t = std::time::Instant::now();
                for _ in 0..iters {
                    std::hint::black_box(set.redact(std::hint::black_box(&text)));
                }
                let secs = t.elapsed().as_secs_f64();
                let mbs = (text.len() * iters) as f64 / secs / 1e6;
                let per = secs / iters as f64 * 1e6;
                println!(
                    "{label:>9} {:>8} B: {mbs:>9.1} MB/s, {per:>9.2} us/call",
                    text.len()
                );
            }
        }
    }
}
