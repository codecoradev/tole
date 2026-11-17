//! Shape-based detection of secrets tole never knew (#597): the shared
//! `uteke-privacy` ruleset (AWS, GitHub, npm, Slack and Google keys, bearer
//! headers, JWTs, PEM blocks, URL passwords, `NAME=value` with an entropy
//! check), the same one uteke runs on its write path, so the two projects
//! cannot drift.
//!
//! [`SecretSet::redact`](super::SecretSet::redact) calls [`redact_shapes`]
//! AFTER the known-value pass, so a value tole holds keeps its `«NAME»`
//! marker and the detector only sees what is left. The mask is the crate's
//! shape-preserving `prefix[REDACTED:<kind>]`; a second pass can only mask more, never less.
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

/// The stand-in of a marker while the detector runs: `qz`, one or more `k`,
/// the marker's index and `x` (`qzkk3x`). Letters and digits only, so a PEM
/// body or a long token keeps its shape around it, and ASCII, so the detector
/// does not read the value as prose. The run of `k` is one longer than any
/// run in the text, so the prefix `qzk..k` cannot occur in the text: every
/// stand-in the detector hands back is one this call created.
const T: &str = "qzk+[0-9]{1,6}x";

/// Always allowed, and ONLY when the marker is the whole secret position:
/// the candidate the detector reports must be exactly one of these shapes.
/// A candidate that merely contains a marker (`API_KEY=«A»<secret>`,
/// `https://bob:«A»<secret>@h`) is not kept, so text that carries a marker
/// cannot smuggle a real secret past the detector. The allowlist is matched
/// against the whole candidate, so every pattern is anchored at both ends.
fn builtin() -> Vec<String> {
    vec![
        // The value of an assignment is a marker and nothing else.
        format!("^{T}$"),
        // `scheme://user:«NAME»@`: the password is the marker.
        format!(r"^[A-Za-z][A-Za-z0-9+.-]*://[^\s:/@«»]*:{T}@$"),
        // `--password=«NAME»`, `--token «NAME»`.
        format!(
            r"(?i)^--(?:password|passwd|token|api-key|apikey|secret|access-token)(?:=|[ \t]+){T}$"
        ),
        // `-u user:«NAME»`.
        format!(r#"(?i)^(?:-u|--user)[ \t]+[^\s:'"«»]+:{T}$"#),
    ]
}

/// Swaps every `«NAME»` for an ASCII stand-in (see [`T`]): the detector
/// treats a non-ASCII value as prose, so a secret glued to a marker would
/// otherwise be skipped. Returns the scan copy, the prefix and the markers.
/// `None` when there is no marker. Never skipped, whatever bytes the text holds.
fn swap_markers(text: &str) -> Option<(String, String, Vec<String>)> {
    if !text.contains('«') {
        return None;
    }
    // One more `k` than the longest `qzkk..` run (any case) in the text.
    let lower = text.to_ascii_lowercase();
    let run = lower
        .match_indices("qz")
        .map(|(i, _)| lower[i + 2..].bytes().take_while(|&b| b == b'k').count())
        .max()
        .unwrap_or(0);
    let prefix = format!("qz{}", "k".repeat(run + 1));
    let (mut out, mut names) = (String::with_capacity(text.len()), Vec::new());
    let mut rest = text;
    while let Some(i) = rest.find('«') {
        let after = &rest[i + '«'.len_utf8()..];
        let name_len = after
            .find(|c: char| !(c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-')))
            .unwrap_or(after.len());
        if (1..=64).contains(&name_len) && after[name_len..].starts_with('»') {
            out.push_str(&rest[..i]);
            out.push_str(&format!("{prefix}{}x", names.len()));
            names.push(rest[i..i + '«'.len_utf8() + name_len + '»'.len_utf8()].to_string());
            rest = &after[name_len + '»'.len_utf8()..];
        } else {
            out.push_str(&rest[..i + '«'.len_utf8()]);
            rest = after;
        }
    }
    out.push_str(rest);
    Some((out, prefix, names))
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
    let (scan, prefix, names) = match swap_markers(text) {
        Some((t, p, n)) => (Cow::Owned(t), p, n),
        None => (Cow::Borrowed(text), String::new(), Vec::new()),
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
        masked = masked.replace(&format!("{prefix}{i}x"), m);
    }
    Cow::Owned(masked)
}
