use serde_json::json;
use serde_json::Value;
use std::cell::Cell;
use std::rc::Rc;
use std::time::Duration;
use tole_core::entry::Entry;
use tole_core::provider::failover::*;
use tole_core::provider::Provider;
use tole_core::provider::ProviderError;
use tole_core::provider::ProviderOutput;

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

fn link(label: &str, replies: Vec<Result<&'static str, &'static str>>) -> (Link, Rc<Cell<usize>>) {
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
