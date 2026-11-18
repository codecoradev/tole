//! `tole webhook test <name>` (#911): one signed `webhook_test` event to one
//! destination, so an operator can check the wiring (the URL, the secret, the
//! receiver's verification) without waiting for a cron run.
//!
//! It is sent directly and once: not through the outbox, no retry, no ledger
//! row. The destination is the one the loaded config names (nothing here takes
//! a URL), the key is read the same way the sender reads it, and the result
//! is a status code or a closed error code, never a URL, a header or any text
//! the receiver sent.

use std::time::Duration;

use serde_json::json;
use tole_core::web::PostError;

use super::send::{Keys, Poster, Reply, Request};
use super::{sign, Destinations, Event};

/// Why a test event could not be sent at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProbeError {
    /// No such destination in the loaded config.
    NoSuchDestination,
    /// The destination's key could not be read (the variable is unset, the file
    /// is gone or too short).
    KeyUnavailable,
}

/// What came of the one request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// The receiver answered with this status.
    Status(u16),
    /// No answer: a code from the closed set (`timeout`, `dns`, `transport`,
    /// `refused_address`, `redirect_refused`, `bad_destination`).
    Failed(&'static str),
}

impl Outcome {
    /// Delivered: a 2xx.
    pub fn ok(&self) -> bool {
        matches!(self, Outcome::Status(s) if (200..300).contains(s))
    }
}

/// The result of a probe: the event id (so the operator can find it in the
/// receiver's log) and what happened.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Probed {
    pub id: String,
    pub outcome: Outcome,
}

/// `test:<16 hex>`, from `nonce` (the caller supplies something unique).
pub fn test_event_id(nonce: &str) -> String {
    format!(
        "test:{}",
        tole_core::webhook::short_digest(nonce.as_bytes())
    )
}

/// The envelope of a test event (schema v1).
pub fn test_body(destination: &str, id: &str, at: i64) -> String {
    json!({
        "v": 1,
        "id": id,
        "type": Event::WebhookTest.as_str(),
        "destination": destination,
        "at": at,
        "data": {
            "message": "tole webhook test",
            "tole_version": env!("CARGO_PKG_VERSION"),
        },
    })
    .to_string()
}

/// Sends one `webhook_test` event to destination `name`. `now` stamps the body
/// and the signature; `nonce` makes the id unique.
pub fn probe(
    dests: &Destinations,
    name: &str,
    now: i64,
    nonce: &str,
    poster: &dyn Poster,
    keys: &dyn Keys,
) -> Result<Probed, ProbeError> {
    let d = dests.get(name).ok_or(ProbeError::NoSuchDestination)?;
    let key = keys.key(d).map_err(|_| ProbeError::KeyUnavailable)?;
    let id = test_event_id(nonce);
    let body = test_body(name, &id, now);
    let headers =
        sign::request_headers(&key, now, &id, Event::WebhookTest.as_str(), body.as_bytes());
    let req = Request {
        url: &d.url,
        body: body.as_bytes(),
        headers: &headers,
        timeout: d.timeout.min(Duration::from_secs(30)),
        policy: d.policy,
    };
    Ok(Probed {
        id,
        outcome: outcome_of(poster.post(&req)),
    })
}

fn outcome_of(r: Result<Reply, PostError>) -> Outcome {
    match r {
        Ok(reply) => Outcome::Status(reply.status),
        Err(PostError::Dns) => Outcome::Failed("dns"),
        Err(PostError::Timeout) => Outcome::Failed("timeout"),
        Err(PostError::Transport(_)) => Outcome::Failed("transport"),
        Err(PostError::RefusedAddress) => Outcome::Failed("refused_address"),
        Err(PostError::Redirect(_)) => Outcome::Failed("redirect_refused"),
        // The URL, the headers or the scheme.
        Err(_) => Outcome::Failed("bad_destination"),
    }
}

#[cfg(test)]
#[path = "probe_tests.rs"]
mod tests;
