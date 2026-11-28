//! #1314: `redact_then_cut` is total (a `max` below the ellipsis used to
//! underflow), never longer than `max`, and cuts on a character boundary.

use tole_core::problem::redact_then_cut;
use tole_core::secrets::SecretSet;

#[test]
fn a_tiny_max_never_panics_and_is_never_exceeded() {
    let set = SecretSet::default();
    for max in 0..8 {
        for text in ["", "abcdefghij", "ééééé", "日本語日本語"] {
            let out = redact_then_cut(text, &set, max);
            assert!(
                out.len() <= max.max(text.len().min(max)),
                "{max} {text}: {out}"
            );
            assert!(text.starts_with(out.trim_end_matches('\u{2026}')), "{out}");
        }
    }
    assert_eq!(redact_then_cut("abcdef", &set, 0), "");
    assert_eq!(redact_then_cut("abcdef", &set, 2), "ab");
    assert_eq!(redact_then_cut("abcdef", &set, 3), "\u{2026}");
    assert_eq!(redact_then_cut("abcdef", &set, 5), "ab\u{2026}");
    assert_eq!(redact_then_cut("abc", &set, 3), "abc");
}

#[test]
fn the_cut_is_taken_after_the_redaction() {
    let set = SecretSet::default().with("K", "secret-value-0123456789");
    let out = redact_then_cut("x secret-value-0123456789 y", &set, 6);
    assert!(!out.contains("secret"), "{out}");
}
