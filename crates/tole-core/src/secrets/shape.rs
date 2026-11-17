//! Shape-based detection of secrets tole never knew (#597): the shared
//! `uteke-privacy` ruleset (AWS, GitHub, npm, Slack and Google keys, bearer
//! headers, JWTs, PEM blocks, URL passwords, `NAME=value` with an entropy
//! check), the same one uteke runs on its write path, so the two projects
//! cannot drift.
//!
//! [`SecretSet::redact`](super::SecretSet::redact) calls [`redact_shapes`]
//! AFTER the known-value pass, so a value tole holds keeps its `«NAME»`
//! marker and the detector only sees what is left. The mask is the crate's
//! shape-preserving `prefix[REDACTED:<kind>]`; the pass is idempotent.
//!
//! False positives are kept through the allowlist: regexes matched against
//! each candidate, installed once by the host with [`set_allowlist`] (the
//! `redaction_allowlist` config list, user layer only: a repository must not
//! be able to switch the detector off for its own values).
//!
//! No IO, no platform assumption: the crate depends on `regex` only.

use std::borrow::Cow;
use std::sync::{Arc, OnceLock, RwLock};
use uteke_privacy::Policy;

static POLICY: RwLock<Option<Arc<Policy>>> = RwLock::new(None);

/// A known-value marker, `«NAME»`, and its stand-in while the detector runs.
const M: &str = "«[A-Za-z0-9_.-]{1,64}»";
const T: &str = "\x01[0-9]{1,6}\x02";

/// Always allowed, and ONLY when the marker is the whole secret position:
/// the candidate the detector reports must be exactly one of these shapes.
/// A candidate that merely contains a marker (`API_KEY=«A»<secret>`,
/// `https://bob:«A»<secret>@h`) is not kept, so text that carries a marker
/// cannot smuggle a real secret past the detector. The allowlist is matched
/// against the whole candidate, so every pattern is anchored at both ends.
fn builtin() -> Vec<String> {
    let mut out = Vec::new();
    for m in [M, T] {
        out.extend([
            // The value of an assignment is a marker and nothing else.
            format!("^{m}$"),
            // `scheme://user:«NAME»@`: the password is the marker.
            format!(r"^[A-Za-z][A-Za-z0-9+.-]*://[^\s:/@«»\x01\x02]*:{m}@$"),
            // `--password=«NAME»`, `--token «NAME»`.
            format!(
                r"(?i)^--(?:password|passwd|token|api-key|apikey|secret|access-token)(?:=|[ \t]+){m}$"
            ),
            // `-u user:«NAME»`.
            format!(r#"(?i)^(?:-u|--user)[ \t]+[^\s:'"«»\x01\x02]+:{m}$"#),
        ]);
    }
    out
}

/// Swaps every `«NAME»` for an ASCII stand-in `\x01<i>\x02`: the detector
/// treats a non-ASCII value as prose, so a secret glued to a marker would
/// otherwise be skipped. `None` when there is no marker, or when the text
/// already holds a control character used as a delimiter (then the text is
/// scanned as it is, and the anchored allow patterns still hold).
fn swap_markers(text: &str) -> Option<(String, Vec<String>)> {
    if !text.contains('«') || text.contains(['\x01', '\x02']) {
        return None;
    }
    let (mut out, mut names) = (String::with_capacity(text.len()), Vec::new());
    let mut rest = text;
    while let Some(i) = rest.find('«') {
        let after = &rest[i + '«'.len_utf8()..];
        let name_len = after
            .find(|c: char| !(c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-')))
            .unwrap_or(after.len());
        if (1..=64).contains(&name_len) && after[name_len..].starts_with('»') {
            out.push_str(&rest[..i]);
            out.push_str(&format!("\x01{}\x02", names.len()));
            names.push(rest[i..i + '«'.len_utf8() + name_len + '»'.len_utf8()].to_string());
            rest = &after[name_len + '»'.len_utf8()..];
        } else {
            out.push_str(&rest[..i + '«'.len_utf8()]);
            rest = after;
        }
    }
    out.push_str(rest);
    Some((out, names))
}

fn policy(user: &[String]) -> Result<Policy, String> {
    let mut all = builtin();
    all.extend_from_slice(user);
    Policy::from_patterns(&all).map_err(|e| e.to_string())
}

/// Installs the allowlist (regex sources); the first invalid one is an
/// error and leaves the current allowlist untouched.
pub fn set_allowlist(patterns: &[String]) -> Result<(), String> {
    let policy = policy(patterns)?;
    if let Ok(mut g) = POLICY.write() {
        *g = Some(Arc::new(policy));
    }
    Ok(())
}

/// Whether every entry compiles (config validation, nothing installed).
pub fn check_allowlist(patterns: &[String]) -> Result<(), String> {
    policy(patterns).map(|_| ())
}

/// The distinct kinds of `[REDACTED:<kind>]` masks in `text`, in order.
pub fn masked_kinds(text: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for part in text.split("[REDACTED:").skip(1) {
        if let Some((kind, _)) = part.split_once(']') {
            if !kind.is_empty() && kind.len() <= 32 && !out.iter().any(|k| k == kind) {
                out.push(kind.to_string());
            }
        }
    }
    out
}

/// Masks every secret shape in `text`; borrowed when there is none.
pub fn redact_shapes(text: &str) -> Cow<'_, str> {
    let set = POLICY.read().ok().and_then(|g| g.clone());
    let (scan, names) = match swap_markers(text) {
        Some((t, n)) => (Cow::Owned(t), n),
        None => (Cow::Borrowed(text), Vec::new()),
    };
    let out = match set {
        Some(p) => uteke_privacy::redact(&scan, &p),
        None => {
            static DEFAULT: OnceLock<Policy> = OnceLock::new();
            let p = DEFAULT.get_or_init(|| policy(&[]).unwrap_or_default());
            uteke_privacy::redact(&scan, p)
        }
    };
    if out.is_clean() {
        return Cow::Borrowed(text);
    }
    let mut masked = out.text;
    for (i, m) in names.iter().enumerate() {
        masked = masked.replace(&format!("\x01{i}\x02"), m);
    }
    Cow::Owned(masked)
}
