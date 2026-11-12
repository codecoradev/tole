//! Webhook signature checks (#582, design `docs/automation-design.md` §6).
//!
//! Pure: the caller passes the route key, the request headers, the raw body
//! and `now`. Two schemes are accepted:
//!
//! - **native**: `X-Tole-Timestamp: <unix seconds>` and
//!   `X-Tole-Signature: sha256=<hex>`, an HMAC-SHA256 over
//!   `<timestamp>.<body>`. The timestamp must be within [`WINDOW_SECS`] of
//!   `now` (either way), so a captured request stops working after five
//!   minutes; within that window the delivery id makes it run once.
//! - **GitHub**: `X-Hub-Signature-256: sha256=<hex>`, an HMAC-SHA256 over the
//!   body. GitHub signs no timestamp, so replay protection is the delivery id
//!   alone, remembered for [`HOOK_RETENTION_SECS`].
//!
//! Every delivery carries an id (`X-Tole-Delivery` / `X-GitHub-Delivery`):
//! the idempotency key the ledger records as `hook_seen`. Comparisons are
//! constant-time. Nothing here formats the key.

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
}

impl Reject {
    /// The HTTP status to answer with.
    pub fn status(self) -> u16 {
        match self {
            Reject::BadDelivery => 400,
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

/// The native `X-Tole-Signature` value for `timestamp` and `body`.
pub fn sign(key: &[u8], timestamp: &str, body: &[u8]) -> String {
    let mut msg = Vec::with_capacity(timestamp.len() + 1 + body.len());
    msg.extend_from_slice(timestamp.as_bytes());
    msg.push(b'.');
    msg.extend_from_slice(body);
    let mac = hmac_sha256(key, &msg);
    let hex: String = mac.iter().map(|b| format!("{b:02x}")).collect();
    format!("sha256={hex}")
}

/// Checks a delivery against `key`; returns its delivery id.
pub fn verify(key: &[u8], h: &Headers, body: &[u8], now: i64) -> Result<String, Reject> {
    let (expected, given, id) = if let Some(sig) = &h.signature {
        let ts = h.timestamp.as_deref().ok_or(Reject::Stale)?;
        let t: i64 = ts.parse().map_err(|_| Reject::Stale)?;
        if now.abs_diff(t) > WINDOW_SECS as u64 {
            return Err(Reject::Stale);
        }
        let mut msg = Vec::with_capacity(ts.len() + 1 + body.len());
        msg.extend_from_slice(ts.as_bytes());
        msg.push(b'.');
        msg.extend_from_slice(body);
        (hmac_sha256(key, &msg), sig, &h.delivery)
    } else if let Some(sig) = &h.github_signature {
        (hmac_sha256(key, body), sig, &h.github_delivery)
    } else {
        return Err(Reject::Unsigned);
    };
    let mac = parse_sig(given).ok_or(Reject::BadSignature)?;
    if !ct_eq(&mac, &expected) {
        return Err(Reject::BadSignature);
    }
    id.as_deref()
        .filter(|d| valid_delivery(d))
        .map(str::to_string)
        .ok_or(Reject::BadDelivery)
}

/// `sha256=<64 hex digits>` (either case) to bytes.
fn parse_sig(s: &str) -> Option<[u8; 32]> {
    let hex = s.trim().strip_prefix("sha256=")?.as_bytes();
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
