use super::*;

/// The pinned vector of `docs/webhook-delivery-design.md` section 3, computed
/// once with an independent HMAC-SHA256 (Python `hmac`), not with this code.
const KEY: &[u8] = b"tole-test-key-0123456789abcdef";
const TS: i64 = 1_795_000_000;
const ID: &str = "cron:daily-1:0123456789abcdef";
const EVENT: &str = "cron_run_finished";
const BODY: &[u8] = br#"{"v":1,"id":"cron:daily-1:0123456789abcdef","type":"cron_run_finished","destination":"gateway","at":1795000000,"data":{"job":"daily","status":"ok","summary":"all good","truncated":false}}"#;
const SIGNATURE: &str = "v2=9d8c931c19daf1192cfb7509c7f10dd86faf8ff0c938ae22d9ca7d99d8764f62";

fn header<'a>(h: &'a [(String, String)], name: &str) -> &'a str {
    h.iter()
        .find(|(k, _)| k == name)
        .map(|(_, v)| v.as_str())
        .unwrap_or_else(|| panic!("no {name}"))
}

#[test]
fn the_signature_is_the_pinned_vector() {
    let h = request_headers(KEY, TS, ID, EVENT, BODY);
    assert_eq!(header(&h, "X-Tole-Signature"), SIGNATURE);
}

#[test]
fn the_headers_are_the_documented_set() {
    let h = request_headers(KEY, TS, ID, EVENT, BODY);
    assert_eq!(header(&h, "Content-Type"), "application/json");
    assert_eq!(header(&h, "X-Tole-Timestamp"), "1795000000");
    assert_eq!(header(&h, "X-Tole-Delivery"), ID);
    assert_eq!(header(&h, "X-Tole-Event"), EVENT);
    assert_eq!(
        header(&h, "User-Agent"),
        format!("tole/{}", env!("CARGO_PKG_VERSION"))
    );
    assert_eq!(h.len(), 6);
}

#[test]
fn no_header_carries_the_secret() {
    let h = request_headers(KEY, TS, ID, EVENT, BODY);
    let key = std::str::from_utf8(KEY).unwrap();
    for (k, v) in &h {
        assert!(!v.contains(key) && !k.contains(key), "{k}");
    }
}

#[test]
fn a_retry_changes_the_timestamp_and_signature_but_not_the_id_or_body() {
    let a = request_headers(KEY, TS, ID, EVENT, BODY);
    let b = request_headers(KEY, TS + 120, ID, EVENT, BODY);
    assert_eq!(header(&a, "X-Tole-Delivery"), header(&b, "X-Tole-Delivery"));
    assert_ne!(
        header(&a, "X-Tole-Timestamp"),
        header(&b, "X-Tole-Timestamp")
    );
    assert_ne!(
        header(&a, "X-Tole-Signature"),
        header(&b, "X-Tole-Signature")
    );
}

#[test]
fn every_signed_part_matters() {
    let base = request_headers(KEY, TS, ID, EVENT, BODY);
    let sig = header(&base, "X-Tole-Signature").to_string();
    let other_key = request_headers(b"another-key-0123456789abcdef", TS, ID, EVENT, BODY);
    let other_id = request_headers(KEY, TS, "cron:daily-2:0123456789abcdef", EVENT, BODY);
    let other_event = request_headers(KEY, TS, ID, "approval_resolved", BODY);
    let other_body = request_headers(KEY, TS, ID, EVENT, b"{}");
    for h in [other_key, other_id, other_event, other_body] {
        assert_ne!(header(&h, "X-Tole-Signature"), sig);
    }
}

#[test]
fn the_headers_pass_the_sender_checks() {
    // A destination URL that is refused for its address proves the headers
    // were accepted first: header errors come before any connection.
    let h = request_headers(KEY, TS, ID, EVENT, BODY);
    let mut p = tole_core::web::PostParams::new("https://127.0.0.1/", BODY);
    p.headers = &h;
    let e = tole_core::web::vetted_post(&p).unwrap_err();
    assert!(
        !matches!(e, tole_core::web::PostError::InvalidHeader(_)),
        "{e}"
    );
}
