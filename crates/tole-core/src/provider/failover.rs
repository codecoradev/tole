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

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;
    use std::rc::Rc;

    /// Scripted provider that counts its calls.
    struct Scripted {
        replies: Vec<Result<&'static str, &'static str>>,
        calls: Rc<Cell<usize>>,
    }

    impl Provider for Scripted {
        fn complete(&mut self, _t: &[Entry]) -> Result<ProviderOutput, ProviderError> {
            let n = self.calls.get();
            self.calls.set(n + 1);
            match self
                .replies
                .get(n)
                .copied()
                .unwrap_or(Err("script exhausted"))
            {
                Ok(text) => Ok(ProviderOutput::Final { text: text.into() }),
                Err(e) => Err(ProviderError(e.into())),
            }
        }
        fn last_usage(&self) -> Option<Value> {
            Some(json!({ "prompt_tokens": self.calls.get() }))
        }
    }

    fn link(
        label: &str,
        replies: Vec<Result<&'static str, &'static str>>,
    ) -> (Link, Rc<Cell<usize>>) {
        let calls = Rc::new(Cell::new(0));
        (
            Link {
                label: label.into(),
                provider: Box::new(Scripted {
                    replies,
                    calls: Rc::clone(&calls),
                }),
            },
            calls,
        )
    }

    fn text(o: ProviderOutput) -> String {
        match o {
            ProviderOutput::Final { text } => text,
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn classification() {
        for yes in [
            "http status: 503 Service Unavailable — upstream down",
            "http status: 500 Internal Server Error — x",
            "http status: 429 Too Many Requests — slow down",
            "io: Connection refused (os error 61)",
            "timeout: global",
            "host not found",
            "stream read: connection reset by peer",
        ] {
            assert!(is_failover_error(yes), "{yes}");
        }
        for no in [
            "http status: 401 Unauthorized — bad key",
            "http status: 403 Forbidden — x",
            "http status: 404 Not Found — no such model",
            "http status: 400 Bad Request — context too long",
            "malformed response: no choices[0].message",
            "malformed tool_call: missing function.name",
        ] {
            assert!(!is_failover_error(no), "{no}");
        }
    }

    #[test]
    fn a_down_primary_fails_over_and_the_route_is_reported() {
        let (p, pc) = link(
            "primary",
            vec![Err("http status: 503 Service Unavailable — down")],
        );
        let (r, rc) = link("reserve", vec![Ok("from reserve"), Ok("again")]);
        let mut f = FailoverProvider::new(vec![p, r]);
        assert_eq!(text(f.complete(&[]).unwrap()), "from reserve");
        let route = f.last_route().unwrap();
        assert_eq!(route["provider"], "reserve");
        assert_eq!(route["failover"]["from"], "primary");
        assert!(route["failover"]["errors"][0]["error"]
            .as_str()
            .unwrap()
            .contains("503"));
        assert_eq!(
            f.last_usage().unwrap()["prompt_tokens"],
            1,
            "usage of the answering provider"
        );
        // Sticky: the next step goes straight to the reserve.
        assert_eq!(text(f.complete(&[]).unwrap()), "again");
        assert_eq!((pc.get(), rc.get()), (1, 2));
        assert!(f.last_route().unwrap().get("failover").is_none());
    }

    #[test]
    fn request_errors_fail_loud_and_never_reach_the_reserve() {
        let (p, _) = link(
            "primary",
            vec![Err("http status: 401 Unauthorized — bad key")],
        );
        let (r, rc) = link("reserve", vec![Ok("unused")]);
        let mut f = FailoverProvider::new(vec![p, r]);
        let e = f.complete(&[]).unwrap_err();
        assert!(e.0.starts_with("http status: 401"), "{}", e.0);
        assert_eq!(rc.get(), 0);
        assert!(f.last_route().is_none());
    }

    #[test]
    fn the_primary_is_retried_after_the_cooldown() {
        let (p, pc) = link(
            "primary",
            vec![
                Err("io: Connection refused (os error 61)"),
                Ok("primary is back"),
            ],
        );
        let (r, _) = link("reserve", vec![Ok("reserve")]);
        let mut f = FailoverProvider::new(vec![p, r]).with_retry_primary_after(Duration::ZERO);
        assert_eq!(text(f.complete(&[]).unwrap()), "reserve");
        assert_eq!(text(f.complete(&[]).unwrap()), "primary is back");
        assert_eq!(pc.get(), 2);
        assert_eq!(f.last_route().unwrap()["provider"], "primary");
    }

    #[test]
    fn when_every_provider_fails_the_last_error_leads() {
        let (p, _) = link("primary", vec![Err("http status: 502 Bad Gateway — a")]);
        let (r, _) = link("reserve", vec![Err("timeout: global")]);
        let mut f = FailoverProvider::new(vec![p, r]);
        let e = f.complete(&[]).unwrap_err().0;
        assert!(e.starts_with("timeout: global"), "{e}");
        assert!(
            e.contains("after failover from primary: http status: 502"),
            "{e}"
        );
    }

    #[test]
    fn a_single_provider_chain_is_transparent() {
        let (p, _) = link(
            "primary",
            vec![Err("http status: 503 Service Unavailable — x")],
        );
        let mut f = FailoverProvider::new(vec![p]);
        assert_eq!(
            f.complete(&[]).unwrap_err().0,
            "http status: 503 Service Unavailable — x"
        );
    }

    #[test]
    fn a_fail_loud_reserve_error_does_not_look_transient() {
        // Primary times out, reserve rejects the key: the combined error
        // must classify by the reserve's error alone, not by the earlier
        // "timeout" text appended after the failover marker.
        let (p, _) = link("primary", vec![Err("timeout: global")]);
        let (r, _) = link(
            "reserve",
            vec![Err("http status: 401 Unauthorized — bad key")],
        );
        let mut f = FailoverProvider::new(vec![p, r]);
        let e = f.complete(&[]).unwrap_err().0;
        assert!(e.contains(FAILOVER_MARKER), "{e}");
        let lead = leading_error(&e);
        assert_eq!(lead, "http status: 401 Unauthorized — bad key");
        assert!(!lead.contains("timeout"));
        assert_eq!(leading_error("plain error"), "plain error");
    }

    #[test]
    fn a_stream_cut_after_output_never_fails_over() {
        let msg = format!("{STREAM_CUT_AFTER_OUTPUT}: connection reset by peer");
        assert!(!is_failover_error(&msg), "{msg}");
        let leaked: &'static str = Box::leak(msg.into_boxed_str());
        let (p, _) = link("primary", vec![Err(leaked)]);
        let (r, rc) = link("reserve", vec![Ok("would duplicate")]);
        let mut f = FailoverProvider::new(vec![p, r]);
        assert!(f.complete(&[]).is_err());
        assert_eq!(rc.get(), 0, "partial output already streamed: no re-stream");
    }

    #[test]
    fn a_failing_sticky_reserve_wraps_back_to_the_primary() {
        let (p, pc) = link(
            "primary",
            vec![
                Err("http status: 503 Service Unavailable — blip"),
                Ok("primary recovered"),
            ],
        );
        let (r, rc) = link(
            "reserve",
            vec![Ok("reserve"), Err("io: Connection refused (os error 61)")],
        );
        let mut f = FailoverProvider::new(vec![p, r]);
        assert_eq!(text(f.complete(&[]).unwrap()), "reserve");
        // Cooldown (300 s) not elapsed, reserve is down: the primary is
        // tried again instead of failing the step.
        assert_eq!(text(f.complete(&[]).unwrap()), "primary recovered");
        assert_eq!((pc.get(), rc.get()), (2, 2));
        let route = f.last_route().unwrap();
        assert_eq!(route["provider"], "primary");
        assert_eq!(route["failover"]["from"], "reserve");
    }

    #[test]
    fn only_the_transports_own_timeout_is_a_timeout_error() {
        for yes in [
            "timeout: global",
            "timeout: recv body",
            "Timeout: global",
            "json: timeout: global at line 1 column 1",
            "timeout: global [after failover from primary: io: refused]",
        ] {
            assert!(is_timeout_error(yes), "{yes}");
        }
        for no in [
            "",
            "http status: 504 \u{2014} gateway timeout",
            "http status: 429 \u{2014} timeout: global",
            "http status: 408 \u{2014} request timeout",
            "io: Connection reset",
            "stream read: timed out",
            "malformed response: timeout: global",
            "the call timeout: global was mentioned",
            "model-capability error: timeout: global",
        ] {
            assert!(!is_timeout_error(no), "{no}");
        }
    }

    #[test]
    fn only_a_failure_before_the_send_is_known_not_to_have_been_sent() {
        for not_sent in [
            "host not found",
            "connection failed",
            "timeout: resolve",
            "timeout: connect",
            "Host Not Found",
            "rustls: invalid certificate",
            "bad uri: nope",
            "model-capability error: the transcript has image input but model m is declared non-vision",
        ] {
            assert!(!request_may_have_been_sent(not_sent), "{not_sent}");
        }
        for sent in [
            "timeout: global",
            "timeout: receive response",
            "timeout: per read",
            "json: timeout: global at line 1 column 1",
            "io: Connection reset by peer",
            "io: connection failed",
            "stream read: timed out",
            "http status: 500 \u{2014} boom",
            "malformed response: no choices[0].message",
            "provider call thread died",
            "something nobody has seen before",
            "",
            // A failover chain: an earlier link may have sent.
            "connection failed [after failover from primary: timeout: global]",
            // The marker words later in a message do not anchor.
            "the host not found it",
            "http status: 502 \u{2014} connection failed",
        ] {
            assert!(request_may_have_been_sent(sent), "{sent}");
        }
    }
}
