//! The headers of one outbound attempt (#911, design section 3).
//!
//! The signature is `tole_core::webhook::sign` (native v2), unchanged, so
//! inbound and outbound cannot drift. The body bytes are fixed when the event
//! is enqueued and identical on every attempt; the timestamp, and so the
//! signature, is taken again for each one. Pure: the clock and the key are
//! arguments.

/// What a request carries besides its body: the four `X-Tole-*` headers, the
/// content type and the user agent. No header carries the secret.
pub fn request_headers(
    key: &[u8],
    timestamp: i64,
    id: &str,
    event: &str,
    body: &[u8],
) -> Vec<(String, String)> {
    let ts = timestamp.to_string();
    let signature = tole_core::webhook::sign(key, &ts, id, Some(event), body);
    vec![
        ("Content-Type".into(), "application/json".into()),
        ("X-Tole-Timestamp".into(), ts),
        ("X-Tole-Delivery".into(), id.into()),
        ("X-Tole-Event".into(), event.into()),
        ("X-Tole-Signature".into(), signature),
        (
            "User-Agent".into(),
            format!("tole/{}", env!("CARGO_PKG_VERSION")),
        ),
    ]
}
