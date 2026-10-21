//! #209 provider failover with the REAL `OpenAiProvider` (so the error
//! classification is checked against real transport/HTTP error text) and
//! the real turn loop (so the route lands in the durable usage ledger).

use serde_json::{json, Value};
use std::io::{Read, Write};
use std::net::TcpListener;
use std::time::Duration;
use tole_core::failover::{is_failover_error, FailoverProvider, Link};
use tole_core::openai::{OpenAiConfig, OpenAiProvider};
use tole_core::provider::Provider;
use tole_core::storage::{JsonlStorage, Storage};
use tole_core::tool::ToolRegistry;
use tole_core::turn::{run_turn, TurnOutcome};

/// Answers every request with `status` and `body` (non-streaming JSON).
fn serve(status: &'static str, body: Value) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    std::thread::spawn(move || {
        for conn in listener.incoming() {
            let Ok(mut s) = conn else { continue };
            let _ = s.set_read_timeout(Some(Duration::from_secs(5)));
            let mut buf = Vec::new();
            let mut chunk = [0u8; 8192];
            // Read headers + body (Content-Length) before answering.
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
            let payload = body.to_string();
            let _ = s.write_all(
                format!(
                    "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{payload}",
                    payload.len()
                )
                .as_bytes(),
            );
        }
    });
    format!("http://{addr}/v1")
}

fn closed_port_url() -> String {
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = l.local_addr().unwrap().port();
    drop(l);
    format!("http://127.0.0.1:{port}/v1")
}

fn provider(url: String) -> OpenAiProvider {
    OpenAiProvider::new(OpenAiConfig::new(url, "m", "sk-test"))
        .with_streaming(false)
        .with_timeout(Duration::from_secs(5))
}

fn final_body(text: &str) -> Value {
    json!({"choices":[{"message":{"role":"assistant","content":text}}],
           "usage":{"prompt_tokens":3,"completion_tokens":1}})
}

fn error_of(url: String) -> String {
    let mut p = provider(url);
    p.complete(&[]).unwrap_err().0
}

#[test]
fn real_error_text_is_classified() {
    let down = error_of(closed_port_url());
    assert!(
        is_failover_error(&down),
        "connection refused must fail over: {down}"
    );
    let busy = error_of(serve("503 Service Unavailable", json!({"error":"down"})));
    assert!(is_failover_error(&busy), "{busy}");
    let limited = error_of(serve("429 Too Many Requests", json!({"error":"slow"})));
    assert!(is_failover_error(&limited), "{limited}");
    let auth = error_of(serve("401 Unauthorized", json!({"error":"bad key"})));
    assert!(
        !is_failover_error(&auth),
        "an auth error must fail loud: {auth}"
    );
    assert!(
        !auth.contains("sk-test"),
        "the key is never in the error: {auth}"
    );
}

fn tmpdir(tag: &str) -> std::path::PathBuf {
    let d = std::env::temp_dir().join(format!("tole-failover-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

#[test]
fn a_turn_survives_a_dead_primary_and_records_the_route() {
    let dir = tmpdir("turn");
    let mut s = JsonlStorage::create(&dir, "t", None).unwrap();
    let mut p = FailoverProvider::new(vec![
        Link {
            label: "primary".into(),
            provider: Box::new(provider(closed_port_url())),
        },
        Link {
            label: "reserve".into(),
            provider: Box::new(provider(serve("200 OK", final_body("from reserve")))),
        },
    ]);
    let reg = ToolRegistry::new();
    let out = run_turn(&mut s, &mut p, &reg, "hi").unwrap();
    assert!(
        matches!(out, TurnOutcome::Final { ref text, .. } if text == "from reserve"),
        "{out:?}"
    );
    let row = &s.usages().last().expect("a usage row").usage;
    assert_eq!(row["prompt_tokens"], 3, "the reserve's own usage is kept");
    assert_eq!(row["tole_route"]["provider"], "reserve");
    assert_eq!(row["tole_route"]["failover"]["from"], "primary");
    // Durable: survives a reopen.
    drop(s);
    let s = JsonlStorage::open(dir.join("t.jsonl")).unwrap();
    assert_eq!(
        s.usages().last().unwrap().usage["tole_route"]["provider"],
        "reserve"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn an_auth_error_on_the_primary_ends_the_turn_without_touching_the_reserve() {
    let dir = tmpdir("auth");
    let mut s = JsonlStorage::create(&dir, "a", None).unwrap();
    let mut p = FailoverProvider::new(vec![
        Link {
            label: "primary".into(),
            provider: Box::new(provider(serve(
                "401 Unauthorized",
                json!({"error":"bad key"}),
            ))),
        },
        Link {
            label: "reserve".into(),
            provider: Box::new(provider(serve("200 OK", final_body("must not answer")))),
        },
    ]);
    let out = run_turn(&mut s, &mut p, &ToolRegistry::new(), "hi").unwrap();
    match out {
        TurnOutcome::ProviderFailed { message } => assert!(message.contains("401"), "{message}"),
        other => panic!("expected ProviderFailed, got {other:?}"),
    }
    let _ = std::fs::remove_dir_all(&dir);
}
