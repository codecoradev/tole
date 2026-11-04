//! #478 bounded provider retry with the REAL `OpenAiProvider`, the real turn
//! loop and a scripted mock HTTP server: 429 / 5xx / empty completions are
//! retried (up to 3 retries), `Retry-After` is honored, every retry is a
//! durable record, and a cancel during a backoff sleep returns promptly.

use serde_json::{json, Value};
use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tole_core::cancel::CancelToken;
use tole_core::openai::{OpenAiConfig, OpenAiProvider};
use tole_core::storage::{JsonlStorage, Storage};
use tole_core::tool::ToolRegistry;
use tole_core::turn::{run_turn, run_turn_with_cancel, TurnOutcome};

const RETRY_RECORD: &str = "provider transient failure, retrying";

#[derive(Clone, Copy)]
enum Reply {
    /// HTTP error status with an optional `Retry-After` (seconds).
    Fail(&'static str, Option<u32>),
    /// 200 with an empty completion (no content, no tool calls).
    Empty,
    Ok,
}

/// Serves `script` in order, then `Reply::Ok` forever. Returns the base
/// url and the request counter.
fn serve(script: Vec<Reply>, streaming: bool) -> (String, Arc<AtomicUsize>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let hits = Arc::new(AtomicUsize::new(0));
    let seen = Arc::clone(&hits);
    std::thread::spawn(move || {
        for conn in listener.incoming() {
            let Ok(mut s) = conn else { continue };
            let _ = s.set_read_timeout(Some(Duration::from_secs(5)));
            let mut buf = Vec::new();
            let mut chunk = [0u8; 8192];
            loop {
                let n = s.read(&mut chunk).unwrap_or(0);
                if n == 0 {
                    break;
                }
                buf.extend_from_slice(&chunk[..n]);
                if let Some(h) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                    let head = String::from_utf8_lossy(&buf[..h]).to_ascii_lowercase();
                    let len: usize = head
                        .lines()
                        .find_map(|l| l.strip_prefix("content-length:"))
                        .and_then(|v| v.trim().parse().ok())
                        .unwrap_or(0);
                    if buf.len() >= h + 4 + len {
                        break;
                    }
                }
            }
            let n = seen.fetch_add(1, Ordering::SeqCst);
            let reply = script.get(n).copied().unwrap_or(Reply::Ok);
            let (status, extra, ctype, body) = match reply {
                Reply::Fail(st, ra) => (
                    st,
                    ra.map(|v| format!("Retry-After: {v}\r\n"))
                        .unwrap_or_default(),
                    "application/json",
                    json!({"error": {"message": "upstream"}}).to_string(),
                ),
                Reply::Empty => (
                    "200 OK",
                    String::new(),
                    ctype(streaming),
                    body(streaming, ""),
                ),
                Reply::Ok => (
                    "200 OK",
                    String::new(),
                    ctype(streaming),
                    body(streaming, "recovered"),
                ),
            };
            let _ = s.write_all(
                format!(
                    "HTTP/1.1 {status}\r\nContent-Type: {ctype}\r\n{extra}Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                )
                .as_bytes(),
            );
        }
    });
    (format!("http://{addr}/v1"), hits)
}

fn ctype(streaming: bool) -> &'static str {
    if streaming {
        "text/event-stream"
    } else {
        "application/json"
    }
}

fn body(streaming: bool, text: &str) -> String {
    if streaming {
        let chunk: Value = json!({"choices":[{"delta":{"content":text}}]});
        format!("data: {chunk}\n\ndata: [DONE]\n\n")
    } else {
        json!({"choices":[{"message":{"role":"assistant","content":text}}]}).to_string()
    }
}

fn provider(url: String, streaming: bool) -> OpenAiProvider {
    OpenAiProvider::new(OpenAiConfig::new(url, "m", "sk-test"))
        .with_streaming(streaming)
        .with_timeout(Duration::from_secs(5))
}

fn session(tag: &str) -> JsonlStorage {
    let dir = std::env::temp_dir().join(format!("tole-478-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    JsonlStorage::create(&dir, tag, None).unwrap()
}

fn retry_records(s: &JsonlStorage) -> Vec<String> {
    s.entries()
        .iter()
        .filter(|e| e.kind.as_str() == "error" && e.payload["error"] == json!(RETRY_RECORD))
        .map(|e| e.payload.to_string())
        .collect()
}

/// Runs a turn against the scripted server; returns outcome, retry
/// records, requests seen and wall time.
fn drive(
    tag: &str,
    script: Vec<Reply>,
    streaming: bool,
) -> (TurnOutcome, Vec<String>, usize, Duration) {
    let (url, hits) = serve(script, streaming);
    let mut p = provider(url, streaming);
    let mut s = session(tag);
    let started = Instant::now();
    let out = run_turn(&mut s, &mut p, &ToolRegistry::new(), "hi").unwrap();
    let took = started.elapsed();
    (out, retry_records(&s), hits.load(Ordering::SeqCst), took)
}

fn assert_final(out: &TurnOutcome) {
    match out {
        TurnOutcome::Final { text, .. } => assert_eq!(text, "recovered"),
        other => panic!("expected Final, got {other:?}"),
    }
}

/// n failures with `Retry-After: 1`: n retries, n durable records, and the
/// header is honored (n waits of about 1 s).
fn transient_then_ok(status: &'static str, n: usize) {
    let (out, records, hits, took) = drive(
        &format!("{}-{n}", &status[..3]),
        vec![Reply::Fail(status, Some(1)); n],
        false,
    );
    assert_final(&out);
    assert_eq!(records.len(), n, "one durable record per retry");
    assert_eq!(hits, n + 1);
    assert!(
        took >= Duration::from_millis(900 * n as u64),
        "Retry-After: 1 not honored, {n} waits took {took:?}"
    );
    assert!(took < Duration::from_secs(n as u64 + 3), "{took:?}");
}

#[test]
fn http_429_once_is_retried_after_retry_after() {
    transient_then_ok("429 Too Many Requests", 1);
}

#[test]
fn http_429_twice_is_retried() {
    transient_then_ok("429 Too Many Requests", 2);
}

#[test]
fn http_429_three_times_is_retried() {
    transient_then_ok("429 Too Many Requests", 3);
}

#[test]
fn http_503_once_is_retried() {
    transient_then_ok("503 Service Unavailable", 1);
}

#[test]
fn http_503_twice_is_retried() {
    transient_then_ok("503 Service Unavailable", 2);
}

#[test]
fn http_503_three_times_is_retried() {
    transient_then_ok("503 Service Unavailable", 3);
}

#[test]
fn retries_are_bounded_at_three() {
    let (out, records, hits, _) = drive(
        "bounded",
        vec![Reply::Fail("503 Service Unavailable", Some(0)); 10],
        false,
    );
    match out {
        TurnOutcome::ProviderFailed { message } => assert!(message.contains("503"), "{message}"),
        other => panic!("expected ProviderFailed, got {other:?}"),
    }
    assert_eq!(records.len(), 3);
    assert_eq!(hits, 4, "1 attempt + 3 retries");
}

#[test]
fn request_errors_are_not_retried() {
    for status in ["401 Unauthorized", "404 Not Found", "400 Bad Request"] {
        let (out, records, hits, _) = drive("loud", vec![Reply::Fail(status, Some(0))], false);
        assert!(
            matches!(out, TurnOutcome::ProviderFailed { .. }),
            "{status}"
        );
        assert!(records.is_empty(), "{status}");
        assert_eq!(hits, 1, "{status}");
    }
}

#[test]
fn empty_completion_is_retried_in_stream_mode() {
    let (out, records, hits, _) = drive("empty-stream", vec![Reply::Empty], true);
    assert_final(&out);
    assert_eq!((records.len(), hits), (1, 2));
}

#[test]
fn empty_completion_is_retried_in_non_stream_mode() {
    // Parity: non-stream used to return Final with empty text (rc 0).
    let (out, records, hits, _) = drive("empty-plain", vec![Reply::Empty], false);
    assert_final(&out);
    assert_eq!((records.len(), hits), (1, 2));
}

#[test]
fn cancel_during_backoff_sleep_returns_promptly() {
    // Retry-After: 30 would sleep the full 30 s; the cancel lands 400 ms in.
    let (url, hits) = serve(vec![Reply::Fail("429 Too Many Requests", Some(30))], false);
    let mut p = provider(url, false);
    let mut s = session("cancel");
    let cancel = CancelToken::new();
    let c = cancel.clone();
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(400));
        c.cancel();
    });
    let started = Instant::now();
    let out = run_turn_with_cancel(&mut s, &mut p, &ToolRegistry::new(), "hi", &cancel).unwrap();
    let took = started.elapsed();
    assert!(matches!(out, TurnOutcome::Cancelled), "{out:?}");
    assert!(
        took < Duration::from_secs(3),
        "slept through the cancel: {took:?}"
    );
    assert_eq!(
        hits.load(Ordering::SeqCst),
        1,
        "no request after the cancel"
    );
    assert_eq!(
        retry_records(&s).len(),
        1,
        "decision recorded before the sleep"
    );
}
