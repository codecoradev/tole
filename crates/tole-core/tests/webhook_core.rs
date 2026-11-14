//! Webhook security core (#582, part 1): HMAC-SHA256, signature checks
//! for the native scheme (`<timestamp>.<body>`, ±300 s) and GitHub's
//! `X-Hub-Signature-256`, delivery ids, and the ledger state behind
//! idempotency and the per-route rate limit (persisted, bounded).

use tole_core::ledger::{Ledger, MemoryStore};
use tole_core::webhook::{self, Headers, Reject, HOOK_RETENTION_SECS, WINDOW_SECS};

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

fn sha2_digest(b: &[u8]) -> Vec<u8> {
    use sha2::Digest;
    sha2::Sha256::digest(b).to_vec()
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
        signature: Some(webhook::sign(KEY, &ts.to_string(), id, None, body)),
        delivery: Some(id.into()),
        ..Headers::default()
    }
}

#[test]
fn a_native_signature_inside_the_window_is_accepted() {
    let body = br#"{"action":"opened"}"#;
    for skew in [0, WINDOW_SECS, -WINDOW_SECS] {
        let v = webhook::verify(KEY, &native(NOW + skew, body, "d-1"), body, NOW).unwrap();
        assert_eq!(
            (v.key.as_str(), v.delivery.as_str()),
            ("d-1", "d-1"),
            "skew {skew}"
        );
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
    // Another key, a truncated or non-hex signature, no `v2=` prefix.
    assert_eq!(
        webhook::verify(b"another-key-another-key-another!", &h, body, NOW),
        Err(Reject::BadSignature)
    );
    let sig = h.signature.clone().unwrap();
    for bad in [&sig[..sig.len() - 2], "v2=zz", &sig["v2=".len()..], ""] {
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
    let v = webhook::verify(KEY, &h, body, NOW).unwrap();
    assert_eq!(v.delivery, "72d3162e-cc78-11e3-81ab-4c9367dc0958");
    assert_eq!(v.key, format!("gh:{}", hex(&sha2_digest(body))));
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
        let mut h = native(NOW, body, "ok");
        h.delivery = Some(bad.to_string());
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

/// #678 (the audit's reproduction): the delivery id was outside both
/// signatures, so one captured delivery replayed as new under a fresh id:
/// native within the window, GitHub forever. Now the native id is signed,
/// and a GitHub delivery's idempotency key is the signed body.
#[test]
fn a_captured_delivery_cannot_be_replayed_under_a_new_id() {
    let body = br#"{"action":"opened"}"#;
    // Native: the captured headers with a new id no longer verify.
    let captured = native(NOW, body, "orig-1");
    let mut replay = captured.clone();
    replay.delivery = Some("replay-2".into());
    assert!(webhook::verify(KEY, &captured, body, NOW + 200).is_ok());
    assert_eq!(
        webhook::verify(KEY, &replay, body, NOW + 200),
        Err(Reject::BadSignature)
    );
    // GitHub: a new delivery header still verifies (GitHub signs only the
    // body), but the idempotency key is the same, so the ledger answers
    // `duplicate` within the retention.
    let gh = |id: &str| Headers {
        github_signature: Some(format!("sha256={}", hex(&webhook::hmac_sha256(KEY, body)))),
        github_delivery: Some(id.into()),
        ..Headers::default()
    };
    let first = webhook::verify(KEY, &gh("g-1"), body, NOW).unwrap();
    let again = webhook::verify(KEY, &gh("g-2"), body, NOW + 86_400).unwrap();
    assert_eq!(first.key, again.key);
    assert_ne!(first.delivery, again.delivery);
    let mut l = Ledger::open(MemoryStore::default()).unwrap();
    assert!(l.record_hook("gh", &first.key, NOW).unwrap());
    assert!(
        !l.record_hook("gh", &again.key, NOW + 86_400).unwrap(),
        "a replay under a new id is a duplicate"
    );
}

/// #678: the native event is signed, so a capture cannot be relabelled to
/// pass a route's `events` list; the fields cannot shift into each other.
#[test]
fn the_native_event_is_signed_and_the_fields_cannot_shift() {
    let body = b"{}";
    let ts = NOW.to_string();
    let signed = |id: &str, event: Option<&str>| Headers {
        timestamp: Some(ts.clone()),
        signature: Some(webhook::sign(KEY, &ts, id, event, body)),
        delivery: Some(id.into()),
        event: event.map(str::to_string),
        ..Headers::default()
    };
    let h = signed("d-1", Some("push"));
    assert_eq!(
        webhook::verify(KEY, &h, body, NOW)
            .unwrap()
            .event
            .as_deref(),
        Some("push")
    );
    for relabel in [Some("release".to_string()), None] {
        let mut r = h.clone();
        r.event = relabel;
        assert_eq!(
            webhook::verify(KEY, &r, body, NOW),
            Err(Reject::BadSignature)
        );
    }
    // An unsigned event added to a delivery signed without one.
    let mut added = signed("d-2", None);
    added.event = Some("push".into());
    assert_eq!(
        webhook::verify(KEY, &added, body, NOW),
        Err(Reject::BadSignature)
    );
    // Moving a `.` between the id and the event changes the message.
    let a = signed("a.b", Some("c"));
    let mut shifted = a.clone();
    shifted.delivery = Some("a".into());
    shifted.event = Some("b.c".into());
    assert_eq!(
        webhook::verify(KEY, &shifted, body, NOW),
        Err(Reject::BadSignature)
    );
    // An event that could carry a separator is refused before any check.
    let mut bad = signed("d-3", Some("push"));
    bad.event = Some("push\nx".into());
    assert_eq!(webhook::verify(KEY, &bad, body, NOW), Err(Reject::BadEvent));
    assert_eq!(Reject::BadEvent.status(), 400);
}

/// #678: the v1 native form signed neither the id nor the event; it is
/// refused with a reason naming the v2 form, as a signature failure.
#[test]
fn a_v1_native_signature_is_refused_as_outdated() {
    let body = br#"{"a":1}"#;
    let ts = NOW.to_string();
    let mut msg = format!("{ts}.").into_bytes();
    msg.extend_from_slice(body);
    let h = Headers {
        timestamp: Some(ts),
        signature: Some(format!("sha256={}", hex(&webhook::hmac_sha256(KEY, &msg)))),
        delivery: Some("d".into()),
        ..Headers::default()
    };
    assert_eq!(
        webhook::verify(KEY, &h, body, NOW),
        Err(Reject::OutdatedScheme)
    );
    assert_eq!(Reject::OutdatedScheme.status(), 401);
    assert!(Reject::OutdatedScheme.reason().contains("v2="));
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
