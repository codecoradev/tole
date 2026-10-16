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

use crate::entry::Entry;
use crate::provider::{Provider, ProviderError, ProviderOutput};
use serde_json::{json, Value};
use std::time::{Duration, Instant};

/// Default time on a reserve before the primary is tried again.
pub const DEFAULT_RETRY_PRIMARY_AFTER: Duration = Duration::from_secs(300);

/// Longest error text kept per failed provider in the route record.
const MAX_ERROR_CHARS: usize = 200;

/// Does this provider error mean "provider unavailable" (fail over) rather
/// than "request wrong" (fail loud)?
pub fn is_failover_error(message: &str) -> bool {
    let m = message.to_ascii_lowercase();
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
        let start = self.active;
        for i in start..self.chain.len() {
            match self.chain[i].provider.complete(transcript) {
                Ok(out) => {
                    if i != self.active {
                        if self.active == 0 {
                            self.left_primary_at = Some(Instant::now());
                        }
                        self.active = i;
                    }
                    self.last_answered = Some(i);
                    return Ok(out);
                }
                Err(e) if i + 1 < self.chain.len() && is_failover_error(&e.0) => {
                    self.last_failed
                        .push((self.chain[i].label.clone(), short(&e.0)));
                }
                Err(e) => {
                    if self.last_failed.is_empty() {
                        return Err(e);
                    }
                    // Keep the last error first: the turn loop's transient
                    // retry classification reads the start of the message.
                    let earlier: Vec<String> = self
                        .last_failed
                        .iter()
                        .map(|(l, m)| format!("{l}: {m}"))
                        .collect();
                    return Err(ProviderError(format!(
                        "{} [after failover from {}]",
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
}
