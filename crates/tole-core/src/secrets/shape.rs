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

/// Always allowed: the `«NAME»` marker of a known-value redaction. Without
/// it the detector would re-mask the marker where it sits in a secret shape
/// (`https://bob:«MCP_URL»@host`) and lose the name.
const MARKER: &str = "«[A-Za-z0-9_.-]{1,64}»";

fn policy(user: &[String]) -> Result<Policy, String> {
    let mut all = vec![MARKER.to_string()];
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

/// Masks every secret shape in `text`; borrowed when there is none.
pub fn redact_shapes(text: &str) -> Cow<'_, str> {
    let set = POLICY.read().ok().and_then(|g| g.clone());
    let out = match set {
        Some(p) => uteke_privacy::redact(text, &p),
        None => {
            static DEFAULT: OnceLock<Policy> = OnceLock::new();
            let p = DEFAULT.get_or_init(|| policy(&[]).unwrap_or_default());
            uteke_privacy::redact(text, p)
        }
    };
    if out.is_clean() {
        Cow::Borrowed(text)
    } else {
        Cow::Owned(out.text)
    }
}
