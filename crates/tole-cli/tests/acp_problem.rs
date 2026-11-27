//! #1023: the typed problem of a settled turn, over the REAL `tole acp`. The
//! ACP `stopReason` enum is fixed by the protocol, so the reply keeps its
//! value (`refusal`) and the problem rides in `result._meta.tole.problem`,
//! with the detail redacted with the session's secrets.

// Spawns the real `tole acp` binary (`shell-tools` feature, #349).
#![cfg(feature = "shell-tools")]

mod common;
use common::TestDir;
use serde_json::{json, Value};
use std::io::{BufRead, Read, Write};
use std::net::TcpListener;
use std::process::{Child, Stdio};
use std::sync::mpsc;
use std::time::Duration;

const LEAKED: &str = "sk-leak-0123456789abcdefXYZ";

/// A provider that answers a final text, or HTTP 400 for a prompt that holds
/// `PROVIDER_FAIL` (the failure text then echoes `LEAKED`).
fn spawn_mock() -> String {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            std::thread::spawn(move || {
                let mut s = stream;
                let (mut buf, mut chunk) = (Vec::new(), [0u8; 16384]);
                let mut body = Value::Null;
                let deadline = std::time::Instant::now() + Duration::from_secs(10);
                while std::time::Instant::now() < deadline {
                    let n = s.read(&mut chunk).unwrap_or(0);
                    buf.extend_from_slice(&chunk[..n]);
                    if let Some(h) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                        if let Ok(v) = serde_json::from_slice::<Value>(&buf[h + 4..]) {
                            body = v;
                            break;
                        }
                    }
                    if n == 0 {
                        break;
                    }
                }
                let streamed = body.get("stream").and_then(Value::as_bool) == Some(true);
                let (status, ctype, out) = if body.to_string().contains("PROVIDER_FAIL") {
                    let msg = format!("scripted failure {LEAKED}");
                    let err = json!({"error": {"message": msg}}).to_string();
                    ("400 Bad Request", "application/json", err)
                } else if streamed {
                    let delta = json!({"choices": [{"index": 0, "delta": {"content": "ok"},
                        "finish_reason": null}]});
                    let usage = json!({"choices": [], "usage": {"prompt_tokens": 1,
                        "completion_tokens": 1, "total_tokens": 2}});
                    let sse = format!("data: {delta}\n\ndata: {usage}\n\ndata: [DONE]\n\n");
                    ("200 OK", "text/event-stream", sse)
                } else {
                    let msg = json!({"role": "assistant", "content": "ok"});
                    let done = json!({"id": "c", "object": "chat.completion", "created": 1,
                        "model": "mock", "choices": [{"index": 0, "message": msg,
                            "finish_reason": "stop"}],
                        "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2}});
                    ("200 OK", "application/json", done.to_string())
                };
                let _ = s.write_all(
                    format!(
                        "HTTP/1.1 {status}\r\nContent-Type: {ctype}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{out}",
                        out.len()
                    )
                    .as_bytes(),
                );
            });
        }
    });
    format!("http://{addr}/v1")
}

struct Acp {
    child: Child,
    _home: TestDir,
    rx: mpsc::Receiver<String>,
}

impl Acp {
    fn spawn(args: &[&str]) -> Self {
        let home = TestDir::new("tole-acp-problem-home");
        let mut child = common::hermetic_command(env!("CARGO_BIN_EXE_tole"), home.path(), &[])
            .args(args)
            .arg("acp")
            .env("TOLE_BASE_URL", spawn_mock())
            .env("TOLE_MODEL", "mock-model")
            .env("TOLE_API_KEY", "sk-test")
            .env("TOLE_TEST_API_KEY", LEAKED)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn tole acp");
        let stdout = child.stdout.take().expect("stdout piped");
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            for line in std::io::BufReader::new(stdout)
                .lines()
                .map_while(Result::ok)
            {
                if tx.send(line).is_err() {
                    break;
                }
            }
        });
        Self {
            child,
            _home: home,
            rx,
        }
    }

    /// The response with `id` (other lines, such as `session/update`, are skipped).
    fn call(&mut self, id: u64, method: &str, params: Value) -> Value {
        let req = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
        writeln!(self.child.stdin.as_mut().expect("stdin"), "{req}").unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(30);
        loop {
            let left = deadline.saturating_duration_since(std::time::Instant::now());
            let line = self.rx.recv_timeout(left).expect("a response in time");
            if let Ok(v) = serde_json::from_str::<Value>(&line) {
                if v.get("id").and_then(Value::as_u64) == Some(id) && v.get("method").is_none() {
                    return v;
                }
            }
        }
    }

    /// One prompt on a fresh session: the `session/prompt` response.
    fn prompt(&mut self, text: &str) -> Value {
        let cwd = TestDir::new("tole-acp-problem");
        self.call(
            1,
            "initialize",
            json!({"protocolVersion": 1, "clientCapabilities": {}}),
        );
        let new = self.call(2, "session/new", json!({"cwd": cwd.to_string_lossy()}));
        let sid = new["result"]["sessionId"]
            .as_str()
            .expect("sessionId")
            .to_string();
        let params = json!({"sessionId": sid, "prompt": [{"type": "text", "text": text}]});
        self.call(3, "session/prompt", params)
    }
}

impl Drop for Acp {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[test]
fn a_provider_failure_keeps_the_refusal_and_carries_the_problem_in_meta() {
    let done = Acp::spawn(&[]).prompt("PROVIDER_FAIL go");
    assert_eq!(done["result"]["stopReason"], "refusal", "{done}");
    let p = &done["result"]["_meta"]["tole"]["problem"];
    assert_eq!(p["kind"], "provider", "{done}");
    assert_eq!(p["retryable"], true, "{done}");
    assert_eq!(p["subject"], Value::Null, "{done}");
    let detail = p["detail"].as_str().expect("detail");
    assert!(detail.contains("scripted failure"), "{done}");
    assert!(
        !done.to_string().contains(LEAKED),
        "the secret left: {done}"
    );
    // `_meta` is the only addition to the result.
    assert_eq!(
        done["result"].as_object().map(|o| o.len()),
        Some(2),
        "{done}"
    );
}

#[test]
fn a_final_answer_has_no_meta() {
    let done = Acp::spawn(&[]).prompt("hello");
    assert_eq!(done["result"], json!({"stopReason": "end_turn"}), "{done}");
}

/// A detail nothing else redacts: a stop gate's reason is the gate's own
/// output, so a secret it prints is redacted only by the egress.
#[cfg(unix)]
#[test]
fn a_secret_in_a_stop_gate_reason_never_leaves_in_the_problem() {
    let dir = TestDir::new("tole-acp-problem-gate");
    let gate = dir.path().join("gate.sh");
    std::fs::write(
        &gate,
        format!("#!/bin/sh\necho denied with {LEAKED}\nexit 2\n"),
    )
    .unwrap();
    let cmd = format!("/bin/sh {}", gate.display());
    let done = Acp::spawn(&["--on-turnend", &cmd]).prompt("just answer");
    assert_eq!(done["result"]["stopReason"], "refusal", "{done}");
    let p = &done["result"]["_meta"]["tole"]["problem"];
    assert_eq!(p["kind"], "stop_gate", "{done}");
    assert!(
        p["detail"].as_str().unwrap_or("").contains("denied with"),
        "{done}"
    );
    assert!(
        !done.to_string().contains(LEAKED),
        "the secret left: {done}"
    );
}
