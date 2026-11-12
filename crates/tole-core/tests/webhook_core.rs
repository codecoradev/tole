//! Webhook security core (#582, part 1): HMAC-SHA256, signature checks
//! for the native scheme (`<timestamp>.<body>`, ±300 s) and GitHub's
//! `X-Hub-Signature-256`, delivery ids, and the ledger state behind
//! idempotency and the per-route rate limit (persisted, bounded).

use tole_core::ledger::{Ledger, MemoryStore};
use tole_core::webhook::{self, Headers, Reject, HOOK_RETENTION_SECS, WINDOW_SECS};

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

#[test]
fn hmac_sha256_matches_rfc_4231() {
    // Test case 1.
    assert_eq!(
        hex(&webhook::hmac_sha256(&[0x0b; 20], b"Hi There")),
        "b0344c61d8db38535ca8afceaf0bf12b881dc200c9833da726e9376c2e32cff7"
    );
    // Test case 2.
    assert_eq!(
        hex(&webhook::hmac_sha256(
            b"Jefe",
            b"what do ya want for nothing?"
        )),
        "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843"
    );
    // Test case 6: a key longer than the block is hashed first.
    assert_eq!(
        hex(&webhook::hmac_sha256(
            &[0xaa; 131],
            b"Test Using Larger Than Block-Size Key - Hash Key First"
        )),
        "60e431591ee0b67f0d8a26aacbf5b77f8e0bc6213728c5140546040f0ee37f54"
    );
}

const KEY: &[u8] = b"a-route-key-of-32-bytes-or-more!";
const NOW: i64 = 1_767_225_600;

fn native(ts: i64, body: &[u8], id: &str) -> Headers {
    Headers {
        timestamp: Some(ts.to_string()),
        signature: Some(webhook::sign(KEY, &ts.to_string(), body)),
        delivery: Some(id.into()),
        ..Headers::default()
    }
}

#[test]
fn a_native_signature_inside_the_window_is_accepted() {
    let body = br#"{"action":"opened"}"#;
    for skew in [0, WINDOW_SECS, -WINDOW_SECS] {
        let id = webhook::verify(KEY, &native(NOW + skew, body, "d-1"), body, NOW);
        assert_eq!(id, Ok("d-1".to_string()), "skew {skew}");
    }
}

#[test]
fn stale_tampered_or_wrongly_keyed_deliveries_are_rejected() {
    let body = br#"{"action":"opened"}"#;
    for skew in [WINDOW_SECS + 1, -WINDOW_SECS - 1] {
        let r = webhook::verify(KEY, &native(NOW + skew, body, "d"), body, NOW);
        assert_eq!(r, Err(Reject::Stale), "skew {skew}");
    }
    // The body or the timestamp changed after signing.
    let h = native(NOW, body, "d");
    assert_eq!(
        webhook::verify(KEY, &h, br#"{"action":"closed"}"#, NOW),
        Err(Reject::BadSignature)
    );
    let mut moved = h.clone();
    moved.timestamp = Some((NOW + 1).to_string());
    assert_eq!(
        webhook::verify(KEY, &moved, body, NOW),
        Err(Reject::BadSignature)
    );
    // Another key, a truncated or non-hex signature, no `sha256=` prefix.
    assert_eq!(
        webhook::verify(b"another-key-another-key-another!", &h, body, NOW),
        Err(Reject::BadSignature)
    );
    let sig = h.signature.clone().unwrap();
    for bad in [
        &sig[..sig.len() - 2],
        "sha256=zz",
        &sig["sha256=".len()..],
        "",
    ] {
        let mut b = h.clone();
        b.signature = Some(bad.to_string());
        assert_eq!(
            webhook::verify(KEY, &b, body, NOW),
            Err(Reject::BadSignature),
            "{bad}"
        );
    }
    // A timestamp that is not a number.
    let mut t = h.clone();
    t.timestamp = Some("soon".into());
    assert_eq!(webhook::verify(KEY, &t, body, NOW), Err(Reject::Stale));
    // Nothing signed at all.
    let none = Headers {
        delivery: Some("d".into()),
        ..Headers::default()
    };
    assert_eq!(
        webhook::verify(KEY, &none, body, NOW),
        Err(Reject::Unsigned)
    );
}

#[test]
fn a_github_signature_is_checked_over_the_body() {
    let body = br#"{"zen":"Keep it logically awesome."}"#;
    let h = Headers {
        github_signature: Some(format!("sha256={}", hex(&webhook::hmac_sha256(KEY, body)))),
        github_delivery: Some("72d3162e-cc78-11e3-81ab-4c9367dc0958".into()),
        ..Headers::default()
    };
    assert_eq!(
        webhook::verify(KEY, &h, body, NOW),
        Ok("72d3162e-cc78-11e3-81ab-4c9367dc0958".to_string())
    );
    assert_eq!(
        webhook::verify(KEY, &h, b"{}", NOW),
        Err(Reject::BadSignature)
    );
}

#[test]
fn a_delivery_id_is_required_and_bounded() {
    let body = b"{}";
    for bad in [
        "",
        "has space",
        "x".repeat(129).as_str(),
        "bad/slash",
        "nul\0",
    ] {
        let h = native(NOW, body, bad);
        assert_eq!(
            webhook::verify(KEY, &h, body, NOW),
            Err(Reject::BadDelivery),
            "{bad:?}"
        );
    }
    let mut h = native(NOW, body, "ok");
    h.delivery = None;
    assert_eq!(
        webhook::verify(KEY, &h, body, NOW),
        Err(Reject::BadDelivery)
    );
    // Status codes: signature problems are 401, a bad id 400.
    assert_eq!(Reject::BadSignature.status(), 401);
    assert_eq!(Reject::Stale.status(), 401);
    assert_eq!(Reject::Unsigned.status(), 401);
    assert_eq!(Reject::BadDelivery.status(), 400);
}

#[test]
fn deliveries_are_counted_per_route_from_the_ledger() {
    let mut l = Ledger::open(MemoryStore::default()).unwrap();
    for i in 0..5 {
        assert!(l.record_hook("gh", &format!("d{i}"), NOW + i).unwrap());
    }
    l.record_hook("other", "d0", NOW).unwrap();
    assert!(!l.record_hook("gh", "d0", NOW + 10).unwrap(), "a repeat");
    assert_eq!(l.state().hooks_since("gh", NOW + 2), 3);
    assert_eq!(l.state().hooks_since("gh", NOW - 1), 5);
    assert_eq!(l.state().hooks_since("nope", NOW - 1), 0);
    // The count survives a restart: it is folded from the log.
    let reopened = Ledger::open(l.store().clone()).unwrap();
    assert_eq!(reopened.state().hooks_since("gh", NOW - 1), 5);
    assert!(reopened.state().hook_seen("gh", "d4"));
}

#[test]
fn seen_deliveries_are_kept_for_the_retention_only() {
    let mut l = Ledger::open(MemoryStore::default()).unwrap();
    l.record_hook("gh", "old", NOW).unwrap();
    // Thousands of later deliveries, past the retention.
    let later = NOW + HOOK_RETENTION_SECS + 1;
    for i in 0..3_000 {
        l.record_hook("gh", &format!("n{i}"), later + i).unwrap();
    }
    let state = l.state();
    assert!(!state.hook_seen("gh", "old"), "pruned after the retention");
    assert!(state.hook_seen("gh", "n0"));
    assert!(state.hooks_tracked() <= 3_000, "{}", state.hooks_tracked());
}
