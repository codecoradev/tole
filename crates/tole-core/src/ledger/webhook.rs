//! Webhook signature checks (#582, design `docs/automation-design.md` §6).
//!
//! Pure: the caller passes the route key, the request headers, the raw body
//! and `now`. Two schemes are accepted:
//!
//! - **native (v2, #678)**: `X-Tole-Timestamp: <unix seconds>`,
//!   `X-Tole-Delivery: <id>`, an optional `X-Tole-Event: <event>` and
//!   `X-Tole-Signature: v2=<hex>`, an HMAC-SHA256 over
//!   `v2\n<timestamp>\n<delivery>\n<event>\n<body>` (an empty event when the
//!   header is absent). The timestamp must be within [`WINDOW_SECS`] of `now`
//!   (either way), so a captured request stops working after five minutes,
//!   and since the delivery id and the event are signed, a capture cannot be
//!   replayed under a fresh id or relabelled as another event within that
//!   window: the id makes it run once. The v1 form (`sha256=` over
//!   `<timestamp>.<body>`) signed neither and is refused
//!   ([`Reject::OutdatedScheme`]).
//! - **GitHub**: `X-Hub-Signature-256: sha256=<hex>`, an HMAC-SHA256 over the
//!   body only. GitHub signs no timestamp, no delivery id and no event, so
//!   the idempotency key is the signed material itself: `gh:` and the
//!   SHA-256 of the body, remembered for [`HOOK_RETENTION_SECS`]. A
//!   captured delivery re-sent under a new `X-GitHub-Delivery` is a
//!   duplicate for that long; after it, it verifies again (no freshness
//!   bound exists in this scheme). The `X-GitHub-Event` header is not
//!   signed: a route's `events` list is a convenience there, not a security
//!   boundary (a `filter` on the payload is signed). Two distinct GitHub
//!   deliveries with byte-identical bodies count as one.
//!
//! Every delivery carries an id (`X-Tole-Delivery` / `X-GitHub-Delivery`).
//! Comparisons are constant-time. Nothing here formats the key.

use sha2::{Digest, Sha256};

/// How far a native timestamp may be from `now`, in seconds.
pub const WINDOW_SECS: i64 = 300;
/// How long a seen delivery id is remembered (the fold drops older ones,
/// which bounds the state; a native delivery that old is stale anyway).
pub const HOOK_RETENTION_SECS: i64 = 7 * 86_400;
/// Longest delivery id.
pub const MAX_DELIVERY_ID: usize = 128;

/// The request headers the check reads (values as received).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Headers {
    /// `X-Tole-Timestamp`.
    pub timestamp: Option<String>,
    /// `X-Tole-Signature`.
    pub signature: Option<String>,
    /// `X-Tole-Delivery`.
    pub delivery: Option<String>,
    /// `X-Hub-Signature-256`.
    pub github_signature: Option<String>,
    /// `X-GitHub-Delivery`.
    pub github_delivery: Option<String>,
    /// `X-Tole-Event` or `X-GitHub-Event`.
    pub event: Option<String>,
}

/// A delivery that passed [`verify`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Verified {
    /// The idempotency key the ledger records: the signed delivery id
    /// (native) or `gh:` and the body's SHA-256 (GitHub).
    pub key: String,
    /// The delivery id as received, for answers and the run's context.
    pub delivery: String,
    /// The event, signed in the native scheme, unsigned from GitHub.
    pub event: Option<String>,
}

/// Why a delivery was refused. The messages say what is wrong, never with
/// what value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reject {
    /// No signature header.
    Unsigned,
    /// The signature does not match (or is malformed).
    BadSignature,
    /// The native timestamp is missing, not a number or outside the window.
    Stale,
    /// The delivery id is missing or invalid.
    BadDelivery,
    /// The event header is not 1-64 of `[A-Za-z0-9._-]`.
    BadEvent,
    /// A native v1 signature (`sha256=`), which signed neither the delivery
    /// id nor the event (#678).
    OutdatedScheme,
}

impl Reject {
    /// The HTTP status to answer with.
    pub fn status(self) -> u16 {
        match self {
            Reject::BadDelivery | Reject::BadEvent => 400,
            _ => 401,
        }
    }

    /// A short reason for the response body.
    pub fn reason(self) -> &'static str {
        match self {
            Reject::Unsigned => "unsigned delivery",
            Reject::BadSignature => "signature check failed",
            Reject::Stale => "timestamp missing or outside the 300 s window",
            Reject::BadDelivery => "delivery id missing or invalid",
            Reject::BadEvent => "event header invalid",
            Reject::OutdatedScheme => {
                "X-Tole-Signature must be v2=<hex> over \
                 v2\\n<timestamp>\\n<delivery>\\n<event>\\n<body>"
            }
        }
    }
}

/// HMAC-SHA256 (RFC 2104).
pub fn hmac_sha256(key: &[u8], msg: &[u8]) -> [u8; 32] {
    const BLOCK: usize = 64;
    let mut k = [0u8; BLOCK];
    if key.len() > BLOCK {
        k[..32].copy_from_slice(&Sha256::digest(key));
    } else {
        k[..key.len()].copy_from_slice(key);
    }
    let pad = |b: u8| k.map(|x| x ^ b);
    let inner = Sha256::new()
        .chain_update(pad(0x36))
        .chain_update(msg)
        .finalize();
    Sha256::new()
        .chain_update(pad(0x5c))
        .chain_update(inner)
        .finalize()
        .into()
}

/// Constant-time equality (length is not secret here).
pub fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// The native v2 signed message: `v2\n<timestamp>\n<delivery>\n<event>\n`
/// then the body. Newlines cannot occur in the three header values (the id
/// and the event are validated, the timestamp is a number), so the fields
/// cannot be shifted into each other.
fn native_message(timestamp: &str, delivery: &str, event: Option<&str>, body: &[u8]) -> Vec<u8> {
    let event = event.unwrap_or("");
    let mut msg =
        Vec::with_capacity(16 + timestamp.len() + delivery.len() + event.len() + body.len());
    for part in ["v2", timestamp, delivery, event] {
        msg.extend_from_slice(part.as_bytes());
        msg.push(b'\n');
    }
    msg.extend_from_slice(body);
    msg
}

/// The native `X-Tole-Signature` value (`v2=<hex>`) for a delivery.
pub fn sign(
    key: &[u8],
    timestamp: &str,
    delivery: &str,
    event: Option<&str>,
    body: &[u8],
) -> String {
    let mac = hmac_sha256(key, &native_message(timestamp, delivery, event, body));
    let hex: String = mac.iter().map(|b| format!("{b:02x}")).collect();
    format!("v2={hex}")
}

/// Checks a delivery against `key`.
pub fn verify(key: &[u8], h: &Headers, body: &[u8], now: i64) -> Result<Verified, Reject> {
    let event = match h.event.as_deref() {
        Some(e) if !valid_event(e) => return Err(Reject::BadEvent),
        e => e.map(str::to_string),
    };
    let valid_id = |id: &Option<String>| {
        id.as_deref()
            .filter(|d| valid_delivery(d))
            .map(str::to_string)
            .ok_or(Reject::BadDelivery)
    };
    if let Some(sig) = &h.signature {
        let ts = h.timestamp.as_deref().ok_or(Reject::Stale)?;
        let t: i64 = ts.parse().map_err(|_| Reject::Stale)?;
        if now.abs_diff(t) > WINDOW_SECS as u64 {
            return Err(Reject::Stale);
        }
        if sig.trim().starts_with("sha256=") {
            return Err(Reject::OutdatedScheme);
        }
        let mac = parse_sig(sig, "v2=").ok_or(Reject::BadSignature)?;
        let id = valid_id(&h.delivery)?;
        let expected = hmac_sha256(key, &native_message(ts, &id, event.as_deref(), body));
        if !ct_eq(&mac, &expected) {
            return Err(Reject::BadSignature);
        }
        Ok(Verified {
            key: id.clone(),
            delivery: id,
            event,
        })
    } else if let Some(sig) = &h.github_signature {
        let mac = parse_sig(sig, "sha256=").ok_or(Reject::BadSignature)?;
        if !ct_eq(&mac, &hmac_sha256(key, body)) {
            return Err(Reject::BadSignature);
        }
        let delivery = valid_id(&h.github_delivery)?;
        let digest: String = Sha256::digest(body)
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        Ok(Verified {
            key: format!("gh:{digest}"),
            delivery,
            event,
        })
    } else {
        Err(Reject::Unsigned)
    }
}

/// 1 to 64 of `[A-Za-z0-9._-]`, the event names a route's `events` lists.
pub fn valid_event(e: &str) -> bool {
    (1..=64).contains(&e.len())
        && e.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
}

/// `<prefix><64 hex digits>` (either case) to bytes.
fn parse_sig(s: &str, prefix: &str) -> Option<[u8; 32]> {
    let hex = s.trim().strip_prefix(prefix)?.as_bytes();
    if hex.len() != 64 {
        return None;
    }
    let digit = |c: u8| (c as char).to_digit(16).map(|d| d as u8);
    let mut out = [0u8; 32];
    for (i, pair) in hex.chunks(2).enumerate() {
        out[i] = digit(pair[0])? << 4 | digit(pair[1])?;
    }
    Some(out)
}

/// 1 to [`MAX_DELIVERY_ID`] of `[A-Za-z0-9._:-]`.
pub fn valid_delivery(id: &str) -> bool {
    (1..=MAX_DELIVERY_ID).contains(&id.len())
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._:-".contains(&b))
}
