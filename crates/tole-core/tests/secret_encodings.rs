//! Known secrets in other encodings and inside larger payloads (#681).
//! Public-API regression tests for `SecretSet::redact`; the unit tests in
//! `secrets.rs` cover the needle construction.

use base64::Engine;
use tole_core::secrets::SecretSet;

const GH: &str = "ghp_0123456789abcdefghijklmnopqrstuvwxyzAB";

fn set() -> SecretSet {
    SecretSet::collect([("GITHUB_TOKEN".to_string(), GH.to_string())], [])
}

/// HTTP Basic `user:token`, a docker config `auth`, a base64'd `.env`: the
/// value sits at any offset of a larger base64 payload.
#[test]
fn base64_of_an_embedded_secret_is_redacted() {
    let s = set();
    let b64 = base64::engine::general_purpose::STANDARD;
    for plain in [
        format!("x-access-token:{GH}"),
        format!("user:{GH}"),
        format!("GITHUB_TOKEN={GH}\n"),
    ] {
        let enc = b64.encode(&plain);
        let out = s
            .redact(&format!("Authorization: Basic {enc}"))
            .into_owned();
        assert!(
            out.contains("«GITHUB_TOKEN»"),
            "base64 form of {plain:?} leaked unredacted: {out}"
        );
    }
}

#[test]
fn hex_json_escape_and_zero_width_forms_are_redacted() {
    let s = set();
    let hex: String = GH.bytes().map(|b| format!("{b:02x}")).collect();
    let slashy = SecretSet::collect([("X_TOKEN".into(), "abc/def/ghi/jkl".into())], []);
    let escaped = r#"{"k":"abc\/def\/ghi\/jkl"}"#;
    let zw = format!("{}\u{200b}{}", &GH[..10], &GH[10..]);
    let mut leaks = Vec::new();
    if !s.redact(&hex).contains('«') {
        leaks.push("hex");
    }
    if !slashy.redact(escaped).contains('«') {
        leaks.push("json \\/ escape");
    }
    if !s.redact(&zw).contains('«') {
        leaks.push("zero-width split");
    }
    assert!(leaks.is_empty(), "missed forms: {leaks:?}");
}

/// The false-positive floor holds: unrelated text, base64 of other values
/// and lone invisible characters are left alone.
#[test]
fn unrelated_text_is_not_redacted() {
    let s = set();
    let b64 = base64::engine::general_purpose::STANDARD;
    for text in [
        format!("Authorization: Basic {}", b64.encode("user:not-the-token")),
        "plain text \u{200b}with a zero-width space".to_string(),
        "6768705f30313233".to_string(), // hex of a prefix only
    ] {
        assert_eq!(s.redact(&text), text);
    }
}
