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

/// Swaps every marker for an ASCII stand-in (see [`T`]): the detector treats
/// a non-ASCII value as prose, so a secret glued to a marker would otherwise
/// be skipped. A marker is any of `known` (the set's exact `«NAME»` strings,
/// whatever characters the name holds, longest first) or a well-formed
/// `«[A-Za-z0-9_.-]{1,64}»` span. Returns the scan copy, the prefix and the
/// markers by index. `None` when there is no marker. Never skipped, whatever
/// bytes the text holds.
fn swap_markers<'a>(text: &'a str, known: &[String]) -> Option<(String, String, Vec<&'a str>)> {
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
    let mut order: Vec<&str> = known.iter().map(String::as_str).collect();
    order.sort_by_key(|m| std::cmp::Reverse(m.len()));
    let (mut out, mut names): (String, Vec<&str>) = (String::with_capacity(text.len()), Vec::new());
    let mut rest = text;
    while let Some(i) = rest.find('«') {
        let here = &rest[i..];
        let len = order
            .iter()
            .find(|m| !m.is_empty() && here.starts_with(**m))
            .map(|m| m.len())
            .or_else(|| generic_marker_len(here));
        match len {
            Some(n) => {
                out.push_str(&rest[..i]);
                out.push_str(&prefix);
                out.push_str(&names.len().to_string());
                out.push('x');
                names.push(&here[..n]);
                rest = &here[n..];
            }
            None => {
                out.push_str(&rest[..i + '«'.len_utf8()]);
                rest = &here['«'.len_utf8()..];
            }
        }
    }
    out.push_str(rest);
    Some((out, prefix, names))
}

/// Length of a well-formed `«NAME»` at the start of `s`, if there is one.
fn generic_marker_len(s: &str) -> Option<usize> {
    let after = s.strip_prefix('«')?;
    let name_len = after
        .find(|c: char| !(c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-')))
        .unwrap_or(after.len());
    ((1..=64).contains(&name_len) && after[name_len..].starts_with('»'))
        .then(|| '«'.len_utf8() + name_len + '»'.len_utf8())
}

/// Puts the markers back in ONE pass: every `<prefix><i>x` whose index this
/// call created becomes its marker; anything else is copied as it is (the
/// prefix cannot occur in the input, but the pass does not rely on it for
/// memory safety or time).
fn restore_markers(masked: &str, prefix: &str, names: &[&str]) -> String {
    let mut out = String::with_capacity(masked.len());
    let mut rest = masked;
    while let Some(i) = rest.find(prefix) {
        out.push_str(&rest[..i]);
        let tail = &rest[i + prefix.len()..];
        let digits = tail.bytes().take_while(u8::is_ascii_digit).count();
        let marker = tail[digits..]
            .starts_with('x')
            .then(|| tail[..digits].parse::<usize>().ok())
            .flatten()
            .and_then(|n| names.get(n));
        match marker {
            Some(m) => {
                out.push_str(m);
                rest = &tail[digits + 1..];
            }
            None => {
                out.push_str(prefix);
                rest = tail;
            }
        }
    }
    out.push_str(rest);
    out
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

/// Masks every secret shape in `text`; borrowed when there is none. `known`
/// are the set's exact `«NAME»` markers, kept out of the detector's way.
pub fn redact_shapes<'a>(text: &'a str, known: &[String]) -> Cow<'a, str> {
    let set = POLICY.read().ok().and_then(|g| g.clone());
    let (scan, prefix, names) = match swap_markers(text, known) {
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
    if names.is_empty() {
        return Cow::Owned(out.text);
    }
    Cow::Owned(restore_markers(&out.text, &prefix, &names))
}
