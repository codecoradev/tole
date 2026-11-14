//! Known-value secret redaction (#595, design `secrets-integrations-design.md`
//! §4): one place that knows which secret VALUES this process holds, and one
//! redactor every egress sink calls before text leaves the process.
//!
//! - [`is_secret_name`] is the single name rule: the child-process env scrub
//!   (ENV-1, `subprocess::scrub_env_for_child`) and [`SecretSet::collect`]
//!   both use it, so "secret-shaped" cannot mean two things.
//! - [`SecretSet::collect`] takes the values of secret-named env vars, the
//!   password of any env value that is a URL with one, plus values handed in
//!   explicitly (the provider key, `${NAME}` header
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
use std::sync::{Arc, OnceLock, RwLock};

static PROCESS: RwLock<Option<SecretSet>> = RwLock::new(None);
static FROM_ENV: OnceLock<SecretSet> = OnceLock::new();

/// Installs the process's secret set (#596): the host does this once, with
/// every secret its egress sinks know (the provider's included), so the
/// session log redacts exactly what the provider is shown.
pub fn set_process_secrets(set: SecretSet) {
    if let Ok(mut g) = PROCESS.write() {
        *g = Some(set);
    }
}

/// The process's secret set: what [`set_process_secrets`] installed, else
/// [`SecretSet::from_env`], built once and cached. Cheap to call (an `Arc`
/// clone): the matcher is never rebuilt per use.
pub fn process_secrets() -> SecretSet {
    if let Some(s) = PROCESS.read().ok().and_then(|g| g.clone()) {
        return s;
    }
    FROM_ENV.get_or_init(SecretSet::from_env).clone()
}

/// Values with fewer characters than this are not collected: an 8-char
/// floor keeps `true`, `1`, `admin` and similar from redacting ordinary text.
pub const MIN_SECRET_LEN: usize = 8;

/// Name substrings that make an env var secret-shaped (case-insensitive):
/// `SECRET_KEY`, `AUTHTOKEN`, `DB_PASSWD` all match (#284).
const SECRET_NAME_MARKERS: [&str; 9] = [
    "API_KEY",
    "APIKEY",
    "SECRET",
    "TOKEN",
    "PASSWORD",
    "PASSWD",
    "PASSPHRASE",
    "CREDENTIAL",
    "PRIVATE_KEY",
];

/// Name endings that make an env var secret-shaped (#682): `DB_PASS`,
/// `PGPASS`, `OPENAI_KEY`, `GH_HOOK_KEY`, `SENTRY_DSN`, `STRIPE_SK`,
/// `SESSION_COOKIE`, `NPM_AUTH`, `AUTH_BEARER`, and the connection-string
/// name `DATABASE_URL` (`TEST_DATABASE_URL` too).
const SECRET_NAME_ENDINGS: [&str; 8] = [
    "PASS",
    "_KEY",
    "_DSN",
    "_SK",
    "_COOKIE",
    "_AUTH",
    "_BEARER",
    "DATABASE_URL",
];

/// Name beginnings that make an env var secret-shaped (#682): `AUTH_BEARER`,
/// `AUTH_TOKEN`-style names whose first word is the credential kind.
const SECRET_NAME_BEGINNINGS: [&str; 2] = ["AUTH_", "BEARER_"];

/// Names the rules sweep up that are not secrets: a non-secret HF tuning
/// knob that the broad `TOKEN` match would otherwise catch, the ssh-agent
/// socket path (`SSH_AUTH_SOCK`, which the `git` tool needs), and the
/// release-signing key FINGERPRINT that official language images export.
const NOT_SECRET: [&str; 3] = ["TOKENIZERS_PARALLELISM", "SSH_AUTH_SOCK", "GPG_KEY"];

/// Name endings that are never secrets although a rule above matches them:
/// public keys, and `BYPASS` (a proxy or feature switch, not a password).
const NOT_SECRET_ENDINGS: [&str; 4] = ["PUBLIC_KEY", "PUB_KEY", "PUBKEY", "BYPASS"];

/// Whether an env var NAME is secret-shaped. The single name rule for the
/// child-env scrub and for [`SecretSet::collect`]. A variable can also be a
/// secret by its VALUE ([`is_secret_var`]).
pub fn is_secret_name(name: &str) -> bool {
    let upper = name.to_ascii_uppercase();
    let shaped = SECRET_NAME_MARKERS.iter().any(|m| upper.contains(m))
        || SECRET_NAME_ENDINGS.iter().any(|e| upper.ends_with(e))
        || SECRET_NAME_BEGINNINGS.iter().any(|b| upper.starts_with(b));
    shaped
        && !NOT_SECRET.contains(&upper.as_str())
        && !NOT_SECRET_ENDINGS.iter().any(|e| upper.ends_with(e))
}

/// Whether an environment variable holds a secret (#682), the rule of the
/// child-env scrub: its name is secret-shaped ([`is_secret_name`]), its
/// value carries a URL password (`postgres://user:pw@host`, whatever the
/// variable is called), or its value is one `known` holds, a secret tole
/// knows by role (a webhook key, the serve token, a `${NAME}` header
/// expansion) under any name.
///
/// Proxy settings (`HTTPS_PROXY`, `https_proxy`, `ALL_PROXY`, any name
/// ending in `_PROXY`) are not removed for their URL password: every
/// network command behind an authenticated proxy needs them. The password
/// is still redacted on egress ([`SecretSet::collect`]).
pub fn is_secret_var(name: &str, value: &str, known: &SecretSet) -> bool {
    let proxy = name.to_ascii_uppercase().ends_with("_PROXY");
    is_secret_name(name) || (!proxy && url_password(value).is_some()) || known.holds(value)
}

/// Secrets this process holds by ROLE rather than by an env var name
/// (#682): webhook keys, the serve token. [`add_role_secret`] records them.
static ROLE: RwLock<Vec<(String, String)>> = RwLock::new(Vec::new());

/// Records a secret the process holds by role, under `name` (an env var
/// name or a label such as `hooks.gh`): it joins the process set
/// ([`process_secrets`], so every egress sink redacts it, and the child
/// scrub removes any variable holding it) and every set a host builds later
/// from [`role_secrets`].
pub fn add_role_secret(name: &str, value: &str) {
    if let Ok(mut g) = ROLE.write() {
        if g.iter().any(|(_, v)| v == value) {
            return;
        }
        g.push((name.to_string(), value.to_string()));
    }
    set_process_secrets(process_secrets().with(name, value));
}

/// The secrets recorded with [`add_role_secret`], in order.
pub fn role_secrets() -> Vec<(String, String)> {
    ROLE.read().map(|g| g.clone()).unwrap_or_default()
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
        // A secret-shaped name holds a secret; any other variable whose
        // value is a URL with a password (`REDIS_URL=redis://:pw@host`)
        // contributes that password (#682).
        let mut from_env: Vec<(String, String)> = env
            .into_iter()
            .filter_map(|(k, v)| {
                if is_secret_name(&k) {
                    Some((k, v))
                } else {
                    let pw = url_password(&v)?.to_string();
                    Some((k, pw))
                }
            })
            .collect();
        from_env.sort();
        Self::build(from_env.into_iter().chain(explicit))
    }

    /// [`SecretSet::collect`] over the process environment (non-UTF-8
    /// entries are skipped: they cannot appear in `&str` text anyway).
    /// The values the web backend headers expand from `${NAME}` count too,
    /// whatever their names (#595).
    pub fn from_env() -> Self {
        let env = std::env::vars_os()
            .filter_map(|(k, v)| Some((k.into_string().ok()?, v.into_string().ok()?)));
        let headers = crate::web::header_secrets(&|k| std::env::var(k).ok());
        Self::collect(env, headers)
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

    /// Whether `value` (trimmed) is one of the collected secrets.
    pub fn holds(&self, value: &str) -> bool {
        let v = value.trim();
        !v.is_empty() && self.inner.entries.iter().any(|(_, e)| e == v)
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
    ///
    /// Text that holds invisible format characters (zero-width space,
    /// joiners, bidi controls, soft hyphen: [`is_invisible`]) is scanned a
    /// second time with those characters removed, and the matches are
    /// mapped back onto the original, so a value split by one is still
    /// found (#681). Text without any is scanned once.
    pub fn redact<'a>(&self, text: &'a str) -> Cow<'a, str> {
        let inner = &*self.inner;
        if inner.needles.is_empty() {
            return Cow::Borrowed(text);
        }
        let mut spans = self.scan(text.as_bytes());
        if has_invisible(text) {
            // `map[i]` is the byte index in `text` of byte `i` of `plain`.
            let mut plain = String::with_capacity(text.len());
            let mut map = Vec::with_capacity(text.len());
            for (at, c) in text.char_indices() {
                if !is_invisible(c) {
                    plain.push(c);
                    map.extend(at..at + c.len_utf8());
                }
            }
            for (s, e, names) in self.scan(plain.as_bytes()) {
                spans.push((map[s], map[e - 1] + 1, names));
            }
            spans = merge_spans(spans);
        }
        if spans.is_empty() {
            return Cow::Borrowed(text);
        }
        let mut out = String::new();
        let mut copied = 0usize;
        for span in spans {
            copied = emit(&mut out, text, copied, span, &inner.markers);
        }
        out.push_str(&text[copied..]);
        Cow::Owned(out)
    }

    /// Merged `(start, end, entries)` spans of every needle in `hay`.
    fn scan(&self, hay: &[u8]) -> Vec<(usize, usize, Vec<usize>)> {
        let inner = &*self.inner;
        let mut spans: Vec<(usize, usize, Vec<usize>)> = Vec::new();
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
            match spans.last_mut() {
                Some((_, s_end, names)) if i < *s_end => {
                    *s_end = (*s_end).max(end);
                    if !names.contains(&nd.entry) {
                        names.push(nd.entry);
                    }
                }
                _ => spans.push((i, end, vec![nd.entry])),
            }
        }
        spans
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

/// Characters that render as nothing (or only reorder text) and that a
/// command or an encoder can insert inside a value without changing what a
/// reader sees: soft hyphen, Arabic letter mark, Mongolian vowel separator,
/// zero-width space and joiners, the bidi marks, overrides and isolates, the
/// word joiner and invisible operators, and the byte order mark.
fn is_invisible(c: char) -> bool {
    matches!(
        c,
        '\u{00AD}'
            | '\u{061C}'
            | '\u{180E}'
            | '\u{200B}'..='\u{200F}'
            | '\u{202A}'..='\u{202E}'
            | '\u{2060}'..='\u{2064}'
            | '\u{2066}'..='\u{206F}'
            | '\u{FEFF}'
    )
}

/// Whether `text` holds an invisible character, decided on the UTF-8 bytes
/// of each one in [`is_invisible`] (no decoding: the common text has none).
fn has_invisible(text: &str) -> bool {
    let b = text.as_bytes();
    b.iter().enumerate().any(|(i, &lead)| {
        let at = |n: usize| b.get(i + n).copied().unwrap_or(0);
        match lead {
            0xC2 => at(1) == 0xAD,
            0xD8 => at(1) == 0x9C,
            0xE1 => at(1) == 0xA0 && at(2) == 0x8E,
            0xE2 => match at(1) {
                0x80 => matches!(at(2), 0x8B..=0x8F | 0xAA..=0xAE),
                0x81 => matches!(at(2), 0xA0..=0xA4 | 0xA6..=0xAF),
                _ => false,
            },
            0xEF => at(1) == 0xBB && at(2) == 0xBF,
            _ => false,
        }
    })
}

type Span = (usize, usize, Vec<usize>);

/// Sort and merge overlapping spans, each entry named once.
fn merge_spans(mut spans: Vec<Span>) -> Vec<Span> {
    spans.sort_by_key(|s| (s.0, std::cmp::Reverse(s.1)));
    let mut out: Vec<Span> = Vec::with_capacity(spans.len());
    for (s, e, names) in spans {
        match out.last_mut() {
            Some((_, o_end, o_names)) if s < *o_end => {
                *o_end = (*o_end).max(e);
                for n in names {
                    if !o_names.contains(&n) {
                        o_names.push(n);
                    }
                }
            }
            _ => out.push((s, e, names)),
        }
    }
    out
}

/// The most needles one value contributes (a bound, so a value never costs
/// more than a fixed number of probes per candidate first byte).
const MAX_FORMS: usize = 24;

/// The forms of a value that can reach a sink: as is; JSON-escaped (inside a
/// serialized JSON string), also with `\/` and `\uXXXX` escapes as other
/// encoders (Python `ensure_ascii`, PHP) write them; base64 standard and
/// URL-safe (no padding: the padded form contains it) alone and embedded at
/// every offset mod 3 of a larger payload (HTTP Basic `user:token`, a docker
/// `auth`, a base64'd `.env`); hex in lower and upper case; and
/// percent-encoded (upper- and lowercase hex). At most [`MAX_FORMS`].
fn forms(value: &str) -> Vec<String> {
    use base64::engine::general_purpose::{STANDARD_NO_PAD, URL_SAFE_NO_PAD};
    let mut v = vec![value.to_string()];
    if let Ok(json) = serde_json::to_string(value) {
        v.push(json[1..json.len() - 1].to_string());
    }
    for slash in [false, true] {
        for unicode in [
            None,
            Some((false, false)),
            Some((true, false)),
            Some((false, true)),
            Some((true, true)),
        ] {
            v.push(json_form(value, slash, unicode));
        }
    }
    v.push(STANDARD_NO_PAD.encode(value));
    v.push(URL_SAFE_NO_PAD.encode(value));
    for engine in [&STANDARD_NO_PAD, &URL_SAFE_NO_PAD] {
        for offset in 0..3 {
            v.push(embedded_base64(engine, value, offset));
        }
    }
    v.push(hex_encode(value, false));
    v.push(hex_encode(value, true));
    // Only the hex digits differ between the two encodings; literal
    // characters keep their case (lowercasing the whole string would miss
    // `Ab%2fcd` and match unrelated lowercase text).
    v.push(percent_encode(value, true));
    v.push(percent_encode(value, false));
    let mut seen = std::collections::HashSet::new();
    v.retain(|f| !f.is_empty() && seen.insert(f.clone()));
    debug_assert!(v.len() <= MAX_FORMS);
    v
}

/// The base64 characters of `value` that do not depend on the bytes around
/// it, when it sits `offset` bytes (mod 3) into a larger payload: the
/// leading characters that mix in the preceding bytes and the trailing
/// partial group (it mixes in the following byte) are dropped. A value of
/// `MIN_SECRET_LEN` bytes keeps at least ten characters.
fn embedded_base64(engine: &impl Engine, value: &str, offset: usize) -> String {
    let mut bytes = vec![0u8; offset];
    bytes.extend_from_slice(value.as_bytes());
    let enc = engine.encode(&bytes);
    // 3 bytes = 4 chars, the first value byte sits at `offset`. Offset 1:
    // chars 0 and 1 hold the preceding byte. Offset 2: chars 0 to 2 do.
    let skip = [0, 2, 3][offset];
    let drop_tail = usize::from(!bytes.len().is_multiple_of(3));
    enc.get(skip..enc.len() - drop_tail)
        .unwrap_or_default()
        .to_string()
}

fn hex_encode(value: &str, upper: bool) -> String {
    use std::fmt::Write;
    let mut out = String::with_capacity(value.len() * 2);
    for b in value.bytes() {
        let _ = if upper {
            write!(out, "{b:02X}")
        } else {
            write!(out, "{b:02x}")
        };
    }
    out
}

/// `value` as the inside of a JSON string, the way other encoders write it:
/// `slash` escapes `/` as `\/`; `unicode = Some((upper, all))` writes every
/// non-ASCII character (every character when `all`) as `\uXXXX` (a surrogate
/// pair above the BMP) with upper- or lowercase hex digits.
fn json_form(value: &str, slash: bool, unicode: Option<(bool, bool)>) -> String {
    use std::fmt::Write;
    let mut out = String::with_capacity(value.len() * 2);
    let mut units = [0u16; 2];
    for c in value.chars() {
        if let Some((upper, all)) = unicode {
            if all || !c.is_ascii() {
                for u in c.encode_utf16(&mut units) {
                    let _ = if upper {
                        write!(out, "\\u{u:04X}")
                    } else {
                        write!(out, "\\u{u:04x}")
                    };
                }
                continue;
            }
        }
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '/' if slash => out.push_str("\\/"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            c if (c as u32) < 0x20 => {
                let _ = write!(out, "\\u{:04x}", c as u32);
            }
            c => out.push(c),
        }
    }
    out
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

    /// #682: secret-bearing names the first rule missed (the audit's list,
    /// less the `*_URL` ones, which are secrets by value).
    #[test]
    fn the_name_rule_covers_the_pass_key_dsn_and_auth_families() {
        for n in [
            "DB_PASS",
            "PGPASS",
            "DATABASE_URL",
            "TEST_DATABASE_URL",
            "SENTRY_DSN",
            "NGINJEN_KEY",
            "GH_HOOK_KEY",
            "HOOK_KEY",
            "STRIPE_SK",
            "OPENAI_KEY",
            "AWS_SECRET",
            "NPM_AUTH",
            "SESSION_COOKIE",
            "AUTH_BEARER",
            "GPG_PASSPHRASE",
            "ssh_key_pass",
        ] {
            assert!(is_secret_name(n), "{n}");
        }
        // Ordinary names stay ordinary, including the near misses.
        for n in [
            "SSH_AUTH_SOCK",
            "GPG_KEY",
            "SSH_PUBLIC_KEY",
            "GIT_AUTHOR_NAME",
            "GIT_AUTHOR_EMAIL",
            "MONKEY",
            "KEYBOARD",
            "TOLE_BASE_URL",
            "REDIS_URL",
            "NO_PROXY",
            "PROXY_BYPASS",
            "XDG_SESSION_TYPE",
            "DBUS_SESSION_BUS_ADDRESS",
            "TERM",
            "LANG",
            "PWD",
            "OLDPWD",
        ] {
            assert!(!is_secret_name(n), "{n}");
        }
    }

    /// #682: a variable is a secret by its value too: a URL password under
    /// any name (not for proxy settings, whose password is still redacted),
    /// or a value tole holds by role.
    #[test]
    fn a_variable_is_a_secret_by_its_value() {
        let held = SecretSet::default().with("hooks.gh", "route-key-0123456789abcdef");
        let pw_url = "redis://:redis-pw-0123456789@cache:6379/0";
        assert!(is_secret_var("REDIS_URL", pw_url, &held));
        assert!(is_secret_var(
            "WHATEVER",
            "route-key-0123456789abcdef",
            &held
        ));
        assert!(is_secret_var(
            "WHATEVER",
            "  route-key-0123456789abcdef\n",
            &held
        ));
        assert!(!is_secret_var(
            "HTTPS_PROXY",
            "http://u:proxy-pw-123@p:3128",
            &held
        ));
        assert!(!is_secret_var(
            "https_proxy",
            "http://u:proxy-pw-123@p:3128",
            &held
        ));
        assert!(!is_secret_var("REDIS_URL", "redis://cache:6379/0", &held));
        assert!(!is_secret_var("EDITOR", "vim", &held));

        // The password of any URL-valued variable is collected, proxies
        // included: redaction covers what the scrub leaves.
        let s = SecretSet::collect(
            [
                ("REDIS_URL".to_string(), pw_url.to_string()),
                (
                    "HTTPS_PROXY".to_string(),
                    "http://u:proxy-pw-123@p:3128".to_string(),
                ),
                ("EDITOR".to_string(), "vim-but-longer".to_string()),
            ],
            [],
        );
        assert_eq!(
            s.redact("redis-pw-0123456789 proxy-pw-123 vim-but-longer"),
            "«REDIS_URL» «HTTPS_PROXY» vim-but-longer"
        );
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

    const GH: &str = "ghp_0123456789abcdefghijklmnopqrstuvwxyzAB";

    #[test]
    fn base64_of_an_embedded_secret_is_redacted() {
        // HTTP Basic / git credential, docker config `auth`, `base64 .env`,
        // at every offset mod 3, in both alphabets (#681).
        use base64::engine::general_purpose::{STANDARD, URL_SAFE};
        let s = set(&[("GITHUB_TOKEN", GH)]);
        for plain in [
            format!("x-access-token:{GH}"),
            format!("user:{GH}"),
            format!("GITHUB_TOKEN={GH}\n"),
            format!("a:{GH}:tail"),
            format!("ab:{GH}!"),
            format!("abc:{GH}??"),
        ] {
            for enc in [STANDARD.encode(&plain), URL_SAFE.encode(&plain)] {
                let out = s
                    .redact(&format!("Authorization: Basic {enc}"))
                    .into_owned();
                assert!(out.contains("«GITHUB_TOKEN»"), "{plain:?} leaked: {out}");
            }
        }
    }

    #[test]
    fn an_embedded_url_safe_base64_form_is_redacted() {
        // A value whose base64 needs `+` and `/` (standard) or `-` and `_`.
        use base64::engine::general_purpose::{STANDARD, URL_SAFE};
        let v = "p@ss/w??rd>>>key=1";
        let s = set(&[("DB_PASSWORD", v)]);
        for pre in ["a:", "ab:", "abc:"] {
            let plain = format!("{pre}{v}!!");
            let (std64, url64) = (STANDARD.encode(&plain), URL_SAFE.encode(&plain));
            assert_ne!(std64, url64, "{plain}: must exercise both alphabets");
            for enc in [std64, url64] {
                let out = s.redact(&format!("Basic {enc}")).into_owned();
                assert!(out.contains("«DB_PASSWORD»"), "{enc} -> {out}");
            }
        }
    }

    #[test]
    fn an_embedded_base64_needle_keeps_the_surrounding_text() {
        use base64::engine::general_purpose::STANDARD;
        let s = set(&[("GITHUB_TOKEN", GH)]);
        let enc = STANDARD.encode(format!("user:{GH}"));
        let out = s.redact(&format!("auth={enc}\n")).into_owned();
        assert!(out.starts_with("auth=") && out.ends_with('\n'), "{out}");
        assert_eq!(out.matches("«GITHUB_TOKEN»").count(), 1, "{out}");
    }

    #[test]
    fn hex_forms_are_redacted_in_both_cases() {
        let s = set(&[("GITHUB_TOKEN", GH)]);
        let lower = hex_encode(GH, false);
        let upper = hex_encode(GH, true);
        assert_ne!(lower, upper);
        assert!(lower.starts_with("67687"), "sanity: hex of ghp_");
        for hex in [lower, upper] {
            assert_eq!(s.redact(&format!("od: {hex}.")), "od: «GITHUB_TOKEN».");
        }
    }

    #[test]
    fn json_slash_and_unicode_escape_forms_are_redacted() {
        let s = set(&[("X_TOKEN", "abc/def/ghi/jkl")]);
        assert_eq!(
            s.redact(r#"{"k":"abc\/def\/ghi\/jkl"}"#),
            r#"{"k":"«X_TOKEN»"}"#
        );
        // Python json.dumps(ensure_ascii=True), either case, a surrogate
        // pair for a character above the BMP, and with `\/` too.
        let s = set(&[("U_SECRET", "pässwörd/😀-ключ")]);
        for text in [
            r#"{"k":"pässwörd/😀-ключ"}"#,
            r#"{"k":"pässwörd/😀-ключ"}"#,
            r#"{"k":"pässwörd\/😀-ключ"}"#,
            r#"{"k":"pässwörd\/😀-ключ"}"#,
            r#"{"k":"pässwörd/😀-ключ"}"#,
        ] {
            assert_eq!(s.redact(text), r#"{"k":"«U_SECRET»"}"#, "{text}");
        }
        // Every character escaped, as some encoders do.
        let all: String = "abcdefgh1"
            .encode_utf16()
            .map(|u| format!("\\u{u:04x}"))
            .collect();
        let s = set(&[("A_TOKEN", "abcdefgh1")]);
        assert_eq!(s.redact(&format!("\"{all}\"")), "\"«A_TOKEN»\"");
    }

    #[test]
    fn json_form_agrees_with_serde_for_the_plain_escape() {
        let v = "q\"b\\s/n\nr\rt\tb\u{8}f\u{c}c\u{1}é";
        let serde = serde_json::to_string(v).unwrap();
        assert_eq!(json_form(v, false, None), serde[1..serde.len() - 1]);
    }

    #[test]
    fn a_secret_split_by_an_invisible_character_is_redacted() {
        let s = set(&[("GITHUB_TOKEN", GH)]);
        for zw in [
            '\u{200b}', '\u{200d}', '\u{feff}', '\u{00ad}', '\u{202e}', '\u{2060}',
        ] {
            let split = format!("{}{zw}{}", &GH[..10], &GH[10..]);
            assert_eq!(
                s.redact(&format!("x={split};")),
                "x=«GITHUB_TOKEN»;",
                "{zw:?}"
            );
        }
        // Several, at the edges and between multibyte neighbours.
        let text = format!("é\u{200b}{}\u{200b}{}\u{200b}é", &GH[..3], &GH[3..]);
        assert_eq!(s.redact(&text), "é\u{200b}«GITHUB_TOKEN»\u{200b}é");
        // A split encoded form is found too (base64 of the value).
        let enc = base64::engine::general_purpose::STANDARD_NO_PAD.encode(GH);
        let split = format!("{}\u{200b}{}", &enc[..9], &enc[9..]);
        assert_eq!(s.redact(&split), "«GITHUB_TOKEN»");
    }

    #[test]
    fn invisible_scanning_merges_with_the_plain_scan_and_stays_idempotent() {
        let s = set(&[("A_TOKEN", "abcdefgh"), ("B_TOKEN", "zzzzyyyy1")]);
        let text = "abcdefgh a\u{200b}bcdefgh zzzz\u{200b}yyyy1 abcdefgh";
        let once = s.redact(text).into_owned();
        assert_eq!(once, "«A_TOKEN» «A_TOKEN» «B_TOKEN» «A_TOKEN»");
        assert_eq!(s.redact(&once), once);
        // Overlap across the two scans names each secret once.
        let s = set(&[("A_TOKEN", "11112222xx"), ("B_TOKEN", "2222xx33334444")]);
        assert_eq!(
            s.redact("<11112222\u{200b}xx33334444>"),
            "<«A_TOKEN»«B_TOKEN»>"
        );
    }

    #[test]
    fn the_byte_check_for_invisible_characters_agrees_with_the_char_check() {
        for c in (0..=0xFFFFu32).filter_map(char::from_u32) {
            let text = format!("a{c}b");
            assert_eq!(has_invisible(&text), is_invisible(c), "U+{:04X}", c as u32);
        }
    }

    #[test]
    fn invisible_characters_alone_redact_nothing() {
        let s = set(&[("A_TOKEN", "abcdefgh")]);
        for text in [
            "plain \u{200b}text\u{feff}",
            "abcdefg\u{200b}x",
            "ab\u{200b}cdefg",
        ] {
            assert!(matches!(s.redact(text), Cow::Borrowed(_)), "{text:?}");
        }
        // A secret that itself holds an invisible character still matches.
        let s = set(&[("Z_TOKEN", "ab\u{200b}cdefgh")]);
        assert_eq!(s.redact("x ab\u{200b}cdefgh y"), "x «Z_TOKEN» y");
    }

    #[test]
    fn needles_per_value_are_bounded_and_long_enough() {
        for v in [
            "abcdefgh",
            GH,
            "p@ss/w??rd>>>key=1",
            "pässwörd/😀-ключ-with-a-long-tail-0123456789",
            "a\"b\\c\nd/e/f/g/h",
        ] {
            let f = forms(v);
            assert!(f.len() <= MAX_FORMS, "{v}: {}", f.len());
            // The false-positive floor: no needle is shorter than the
            // shortest value (8 characters), embedded base64 included.
            for n in &f {
                assert!(n.chars().count() >= MIN_SECRET_LEN, "{v}: {n}");
            }
        }
        assert_eq!(forms("abcdefgh").iter().map(|n| n.len()).min(), Some(8));
        assert!(forms("abcdefgh").iter().all(|n| n.len() >= 8));
    }

    #[test]
    fn a_large_text_with_invisible_characters_is_still_linear() {
        // Not a timing gate: 4 MiB with an invisible character in every
        // line must finish and redact every planted value.
        let s = set(&[("GITHUB_TOKEN", GH)]);
        let unit = format!("{}\u{200b}{}\n", &GH[..7], &GH[7..]);
        let text = unit.repeat(4 * 1024 * 1024 / unit.len());
        let out = s.redact(&text);
        assert_eq!(
            out.matches("«GITHUB_TOKEN»").count(),
            text.matches('\n').count()
        );
        assert!(!out.contains("ghp_"));
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
