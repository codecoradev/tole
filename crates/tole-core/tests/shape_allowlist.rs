//! #597: a false positive is kept through the allowlist (regexes matched
//! against each candidate). The allowlist is process-wide, so this test
//! lives in its own binary.

use tole_core::secrets::{shape, SecretSet};

#[test]
fn an_allowlisted_shape_is_kept_and_everything_else_still_masked() {
    let fixture = ["ghp", "_0000000000000000000000000000000000FX"].concat();
    let real = ["ghp", "_aB3dE5gH7jK9mN1pQ3rS5tU7vW9xY1zA3bC5"].concat();
    let s = SecretSet::default();
    let text = format!("fixture {fixture} real {real}");
    // Before: both are masked.
    assert!(!s.redact(&text).contains(&fixture));
    shape::set_allowlist(&["^ghp_0{20}".to_string()]).unwrap();
    let out = s.redact(&text);
    assert!(out.contains(&fixture), "allowlisted value kept: {out}");
    assert!(!out.contains(&real), "other tokens still masked: {out}");
    // An invalid regex is an error and leaves the allowlist as it was.
    assert!(shape::set_allowlist(&["(".to_string()]).is_err());
    assert!(s.redact(&text).contains(&fixture));
    // Clearing restores the full ruleset.
    shape::set_allowlist(&[]).unwrap();
    assert!(!s.redact(&text).contains(&fixture));
}
