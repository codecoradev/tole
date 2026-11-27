//! Retry classification of provider errors (#58, #478).

use super::is_transient_provider_error;

#[test]
fn only_the_last_failover_error_decides_a_retry() {
    assert!(is_transient_provider_error("timeout: global"));
    assert!(is_transient_provider_error(
        "http status: 429 Too Many Requests — x"
    ));
    assert!(!is_transient_provider_error(
        "http status: 401 Unauthorized — bad key [after failover from primary: timeout: global]"
    ));
    assert!(is_transient_provider_error(
        "timeout: global [after failover from primary: http status: 502 Bad Gateway — a]"
    ));
}

#[test]
fn http_status_errors_are_classified_by_code_not_body() {
    for (msg, want) in [
        ("http status: 429 Too Many Requests — rate limited", true),
        ("http status: 429", true),
        (
            r#"http status: 400 Bad Request — {"error":{"message":"resulted in 142900 tokens"}}"#,
            false,
        ),
        (
            r#"http status: 422 Unprocessable — {"error":{"message":"invalid timeout value"}}"#,
            false,
        ),
        ("http status: 401 Unauthorized — token timeout, re-login", false),
        ("http status: 400 Bad Request — unknown model id gpt-4290", false),
        ("http status: 503 Service Unavailable — upstream", true),
        ("http status: 500", true),
        ("http status: 599 x", true),
        ("http status: 600 x", false),
        ("http status: 404 Not Found — 503 upstream", false),
        ("malformed response: empty stream (no content, no tool calls)", true),
        ("malformed response: no choices[0].message", false),
        ("io: Connection refused (os error 111)", true),
        ("http status: 4290 Weird", false),
        ("timeout: global", true),
        ("stream read: timeout reading", true),
        (
            "http status: 429 Too Many Requests — x [after failover from primary: http status: 401 Unauthorized — y]",
            true,
        ),
        (
            "http status: 400 Bad Request — token timeout [after failover from primary: timeout: global]",
            false,
        ),
    ] {
        assert_eq!(is_transient_provider_error(msg), want, "{msg}");
    }
}

#[test]
fn retry_delay_backs_off_honors_and_caps_retry_after() {
    use super::retry_delay;
    use std::time::Duration;
    for (used, base) in [(0, 1.0), (1, 2.0), (2, 4.0)] {
        let d = retry_delay("timeout: global", used).as_secs_f64();
        assert!((base..=base * 1.25).contains(&d), "retry {used}: {d}");
    }
    let hdr = |n: &str| format!("http status: 429 Too Many Requests (retry-after: {n}) — x");
    assert_eq!(retry_delay(&hdr("7"), 0), Duration::from_secs(7));
    assert_eq!(retry_delay(&hdr("0"), 2), Duration::ZERO);
    assert_eq!(retry_delay(&hdr("3600"), 0), Duration::from_secs(30));
    // HTTP-date form is not parsed: plain backoff.
    let d = retry_delay(&hdr("Wed, 21 Oct 2026 07:28:00 GMT"), 0);
    assert!(d >= Duration::from_secs(1) && d < Duration::from_secs(2));
    // A hint on an earlier failover error does not count.
    let d = retry_delay(
        "timeout: global [after failover from primary: http status: 429 (retry-after: 9)]",
        0,
    );
    assert!(d < Duration::from_secs(2));
}
