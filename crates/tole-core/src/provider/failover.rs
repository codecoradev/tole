//! Provider failover chain (#209).
//!
//! `FailoverProvider` wraps an ordered chain of providers (primary first).
//! When the active provider fails with an error that means "this provider
//! is unavailable" — connection failure, timeout, a dropped stream, HTTP
//! 5xx or 429 — the same step is retried on the next provider in the chain.
//! Errors that mean "this request is wrong" (401/403/404/400, a malformed
//! response) fail loud and never fail over: a reserve would only hide a
//! misconfiguration.
//!
//! The provider that answered stays active (no flapping, no repeated slow
//! failures on a dead primary); after `retry_primary_after` the primary is
//! tried again. Each answered step reports `last_route()` — the provider
//! that answered and, when it took a failover, where from and why — which
//! the turn loop stores under `tole_route` in the step's usage row, so a
//! switch is durable and auditable.

use crate::cancel::CancelToken;
use crate::entry::Entry;
use crate::provider::{Provider, ProviderError, ProviderOutput};
use serde_json::{json, Value};
use std::time::{Duration, Instant};

/// Default time on a reserve before the primary is tried again.
pub const DEFAULT_RETRY_PRIMARY_AFTER: Duration = Duration::from_secs(300);

/// Longest error text kept per failed provider in the route record.
const MAX_ERROR_CHARS: usize = 200;

/// Separates the last provider's error from the earlier ones in a
/// combined error: `"<last error> [after failover from <label>: ...]"`.
pub const FAILOVER_MARKER: &str = " [after failover from ";

/// Error prefix of a stream that broke after a live-token sink already
/// showed part of the answer. Never fails over: the reserve would stream
/// a whole new answer after the partial one.
pub const STREAM_CUT_AFTER_OUTPUT: &str = "stream cut after output";

/// The last provider's error of a combined failover error (the whole
/// message when no failover happened). Retry classification must read
/// only this part: the earlier errors may say "timeout" or "429" while
/// the last one is a fail-loud 401.
pub fn leading_error(message: &str) -> &str {
    message
        .split_once(FAILOVER_MARKER)
        .map_or(message, |(head, _)| head)
}

/// Is this error the provider's own request timeout (#756)? Only the
/// transport's `timeout: <phase>` text counts, as the whole message or after
/// the `json: ` prefix of a reply cut while it was read. Anchored on purpose:
/// an `http status: 504 ...` error (a 4xx/5xx the server answered, whatever
/// its body says) and any other failure never qualify.
pub fn is_timeout_error(message: &str) -> bool {
    let m = leading_error(message).trim_start().to_ascii_lowercase();
    let m = m.strip_prefix("json: ").unwrap_or(&m);
    m.starts_with("timeout: ")
}

/// Error texts (lowercase prefixes of the error) that prove the request never
/// left: the failure came before anything was sent. The list is closed on
/// purpose: anything else is treated as sent.
const NOT_SENT_PREFIXES: &[&str] = &[
    "host not found",
    "connection failed",
    "timeout: resolve",
    "timeout: connect",
    "bad uri",
    "invalid proxy url",
    "connect proxy failed",
    "tls required",
    "configured for https only",
    "rustls: ",
    "native-tls: ",
    "pem: ",
    "model-capability error: the transcript has image input",
];

/// Could the request of a failed call have reached the provider (#804)? Then
/// the provider may have generated, and billed, any part of the reply while
/// reporting no usage to us: a host with a hard budget charges such a call
/// its estimate. Conservative in the safe direction: only the closed list of
/// pre-send failures (a refused connection, DNS, the TLS handshake, a
/// rejected URL) says `false`; every other text, an unknown one included, and
/// any failover chain (an earlier link may have sent) says `true`.
pub fn request_may_have_been_sent(message: &str) -> bool {
    if message.contains(FAILOVER_MARKER) {
        return true;
    }
    let m = message.trim_start().to_ascii_lowercase();
    !NOT_SENT_PREFIXES.iter().any(|p| m.starts_with(p))
}

/// Does this provider error mean "provider unavailable" (fail over) rather
/// than "request wrong" (fail loud)?
pub fn is_failover_error(message: &str) -> bool {
    let m = message.to_ascii_lowercase();
    if m.starts_with(STREAM_CUT_AFTER_OUTPUT) {
        return false;
    }
    if let Some(rest) = m.strip_prefix("http status: ") {
        let code: u16 = rest
            .chars()
            .take_while(char::is_ascii_digit)
            .collect::<String>()
            .parse()
            .unwrap_or(0);
        return code == 429 || (500..600).contains(&code);
    }
    if m.starts_with("malformed") {
        return false;
    }
    [
        "timeout",
        "timed out",
        "io:",
        "connection",
        "host not found",
        "dns",
        "stream read",
        "reset",
        "broken pipe",
        "unexpected eof",
    ]
    .iter()
    .any(|k| m.contains(k))
}

/// One provider in the chain, with the label recorded in `tole_route`.
pub struct Link {
    pub label: String,
    pub provider: Box<dyn Provider>,
}

pub struct FailoverProvider {
    chain: Vec<Link>,
    active: usize,
    left_primary_at: Option<Instant>,
    retry_primary_after: Duration,
    last_answered: Option<usize>,
    /// `[(label, error)]` of the providers that failed before the last
    /// answer of the current step.
    last_failed: Vec<(String, String)>,
}

impl FailoverProvider {
    /// `chain[0]` is the primary. Panics on an empty chain (a host bug).
    pub fn new(chain: Vec<Link>) -> Self {
        assert!(!chain.is_empty(), "a failover chain needs a provider");
        Self {
            chain,
            active: 0,
            left_primary_at: None,
            retry_primary_after: DEFAULT_RETRY_PRIMARY_AFTER,
            last_answered: None,
            last_failed: Vec::new(),
        }
    }

    pub fn with_retry_primary_after(mut self, after: Duration) -> Self {
        self.retry_primary_after = after;
        self
    }

    fn answered(&self) -> Option<&dyn Provider> {
        self.last_answered.map(|i| self.chain[i].provider.as_ref())
    }
}

fn short(s: &str) -> String {
    let mut out: String = s.chars().take(MAX_ERROR_CHARS).collect();
    if s.chars().count() > MAX_ERROR_CHARS {
        out.push('…');
    }
    out
}

impl Provider for FailoverProvider {
    fn complete(&mut self, transcript: &[Entry]) -> Result<ProviderOutput, ProviderError> {
        self.complete_with_cancel(transcript, &CancelToken::default())
    }

    fn complete_with_cancel(
        &mut self,
        transcript: &[Entry],
        cancel: &CancelToken,
    ) -> Result<ProviderOutput, ProviderError> {
        // Back to the primary once the cooldown has passed.
        if self.active > 0
            && self
                .left_primary_at
                .is_some_and(|t| t.elapsed() >= self.retry_primary_after)
        {
            self.active = 0;
            self.left_primary_at = None;
        }
        self.last_answered = None;
        self.last_failed.clear();
        // Active link first, then the rest in chain order, wrapping round:
        // a sticky reserve that fails before the cooldown ends still lets
        // an earlier (maybe recovered) provider answer the step.
        let n = self.chain.len();
        let order: Vec<usize> = (0..n).map(|k| (self.active + k) % n).collect();
        for (pos, &i) in order.iter().enumerate() {
            let has_next = pos + 1 < n;
            match self.chain[i]
                .provider
                .complete_with_cancel(transcript, cancel)
            {
                Ok(out) => {
                    if i != self.active {
                        if i == 0 {
                            self.left_primary_at = None;
                        } else if self.active == 0 {
                            self.left_primary_at = Some(Instant::now());
                        }
                        self.active = i;
                    }
                    self.last_answered = Some(i);
                    return Ok(out);
                }
                Err(e) if has_next && !cancel.is_cancelled() && is_failover_error(&e.0) => {
                    self.last_failed
                        .push((self.chain[i].label.clone(), short(&e.0)));
                }
                Err(e) => {
                    if self.last_failed.is_empty() {
                        return Err(e);
                    }
                    // The last error leads; see `leading_error`.
                    let earlier: Vec<String> = self
                        .last_failed
                        .iter()
                        .map(|(l, m)| format!("{l}: {m}"))
                        .collect();
                    return Err(ProviderError(format!(
                        "{}{FAILOVER_MARKER}{}]",
                        e.0,
                        earlier.join("; ")
                    )));
                }
            }
        }
        unreachable!("the loop returns on the last provider")
    }

    fn last_usage(&self) -> Option<Value> {
        self.answered().and_then(Provider::last_usage)
    }

    fn last_reasoning(&self) -> Option<String> {
        self.answered().and_then(Provider::last_reasoning)
    }

    fn last_wire_stats(&self) -> Option<Value> {
        self.answered().and_then(Provider::last_wire_stats)
    }

    fn last_degraded(&self) -> Option<Value> {
        self.answered().and_then(Provider::last_degraded)
    }

    fn last_extra_tool_calls(&self) -> Vec<(String, Value)> {
        self.answered()
            .map(Provider::last_extra_tool_calls)
            .unwrap_or_default()
    }

    fn last_route(&self) -> Option<Value> {
        let i = self.last_answered?;
        let mut route = json!({ "provider": self.chain[i].label });
        if !self.last_failed.is_empty() {
            route["failover"] = json!({
                "from": self.last_failed[0].0,
                "errors": self
                    .last_failed
                    .iter()
                    .map(|(l, m)| json!({ "provider": l, "error": m }))
                    .collect::<Vec<_>>(),
            });
        }
        Some(route)
    }
}
