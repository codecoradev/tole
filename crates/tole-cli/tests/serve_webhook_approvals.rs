//! Approval webhooks on the real `tole serve` (#911, part 5d): a write that
//! needs approval is announced to a destination that lists
//! `approval_requested`, and the operator's decision is announced as
//! `approval_resolved`. The receiver recomputes the v2 signature over the
//! bytes it got. The write's path carries the destination key, so the
//! description in the event must come out redacted.

#![cfg(feature = "shell-tools")]

mod common;
use common::TestDir;
use serde_json::{json, Value};
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::Duration;

use tole_core::webhook;

const SIGN_KEY: &str = "approval-destination-key-3c4d5e6f7a8b";
const SIGN_VAR: &str = "PUSHSIG_E2E_B";

struct Got {
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

impl Got {
    fn header(&self, k: &str) -> &str {
        self.headers
            .iter()
            .find(|(n, _)| n.eq_ignore_ascii_case(k))
            .map(|(_, v)| v.as_str())
            .unwrap_or_else(|| panic!("no header {k}"))
    }
    fn json(&self) -> Value {
        serde_json::from_slice(&self.body).unwrap()
    }
}

/// Answers every request 200 and hands it to the test.
fn receiver() -> (u16, mpsc::Receiver<Got>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        for mut s in listener.incoming().flatten() {
            let tx = tx.clone();
            std::thread::spawn(move || {
                let mut buf = Vec::new();
                let mut chunk = [0u8; 4096];
                let (head_end, len) = loop {
                    let n = s.read(&mut chunk).unwrap_or(0);
                    if n == 0 {
                        return;
                    }
                    buf.extend_from_slice(&chunk[..n]);
                    if let Some(p) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                        let head = String::from_utf8_lossy(&buf[..p]).to_ascii_lowercase();
                        let len = head
                            .lines()
                            .find_map(|l| l.strip_prefix("content-length:")?.trim().parse().ok())
                            .unwrap_or(0usize);
                        break (p + 4, len);
                    }
                };
                while buf.len() < head_end + len {
                    let n = s.read(&mut chunk).unwrap_or(0);
                    if n == 0 {
                        break;
                    }
                    buf.extend_from_slice(&chunk[..n]);
                }
                let head = String::from_utf8_lossy(&buf[..head_end - 4]).to_string();
                let headers = head
                    .lines()
                    .skip(1)
                    .filter_map(|l| l.split_once(':'))
                    .map(|(k, v)| (k.trim().to_string(), v.trim().to_string()))
                    .collect();
                let body = buf[head_end.min(buf.len())..].to_vec();
                let _ = tx.send(Got { headers, body });
                let _ = write!(
                    s,
                    "HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                );
            });
        }
    });
    (port, rx)
}

/// A provider: the first request asks for a `write_file` whose path holds
/// the destination key (the description names the path); once a tool result is in, it answers with text.
fn mock_provider() -> String {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}/v1", listener.local_addr().unwrap());
    std::thread::spawn(move || {
        for mut s in listener.incoming().flatten() {
            std::thread::spawn(move || {
                let mut buf = Vec::new();
                let mut chunk = [0u8; 16384];
                let deadline = std::time::Instant::now() + Duration::from_secs(10);
                let mut body = Value::Null;
                while std::time::Instant::now() < deadline {
                    let n = s.read(&mut chunk).unwrap_or(0);
                    buf.extend_from_slice(&chunk[..n]);
                    if let Some(p) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                        if let Ok(v) = serde_json::from_slice::<Value>(&buf[p + 4..]) {
                            body = v;
                            break;
                        }
                    }
                    if n == 0 {
                        break;
                    }
                }
                let streamed = body["stream"].as_bool() == Some(true);
                let msgs = body["messages"].as_array().cloned().unwrap_or_default();
                let tool_result = msgs.iter().any(|m| m["role"] == "tool");
                let args = json!({"path": format!("{SIGN_KEY}.txt"), "content": "x"}).to_string();
                let payload = if streamed {
                    let delta = if tool_result {
                        json!({"content": "done"})
                    } else {
                        json!({"tool_calls": [{"index": 0, "id": "call_1", "function":
                            {"name": "write_file", "arguments": args}}]})
                    };
                    format!(
                        "data: {}\n\ndata: {}\n\ndata: [DONE]\n\n",
                        json!({"choices": [{"index": 0, "delta": delta, "finish_reason": null}]}),
                        json!({"choices": [], "usage": {"prompt_tokens": 1,
                               "completion_tokens": 1, "total_tokens": 2}})
                    )
                } else {
                    let msg = if tool_result {
                        json!({"role": "assistant", "content": "done"})
                    } else {
                        json!({"role": "assistant", "content": null, "tool_calls": [
                            {"id": "call_1", "type": "function",
                             "function": {"name": "write_file", "arguments": args}}]})
                    };
                    json!({"id": "c", "object": "chat.completion", "created": 1,
                        "model": "mock", "choices": [{"index": 0, "message": msg,
                        "finish_reason": if tool_result { "stop" } else { "tool_calls" }}],
                        "usage": {"prompt_tokens": 1, "completion_tokens": 1,
                                  "total_tokens": 2}})
                    .to_string()
                };
                let kind = if streamed {
                    "text/event-stream"
                } else {
                    "application/json"
                };
                let _ = write!(
                    s,
                    "HTTP/1.1 200 OK\r\nContent-Type: {kind}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{payload}",
                    payload.len()
                );
            });
        }
    });
    url
}

fn http(port: u16, method: &str, path: &str, body: Option<&str>) -> (u16, Value) {
    let mut s = TcpStream::connect(("127.0.0.1", port)).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(20))).unwrap();
    let b = body.unwrap_or("");
    write!(
        s,
        "{method} {path} HTTP/1.1\r\nAuthorization: Bearer t0ken\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{b}",
        b.len()
    )
    .unwrap();
    let mut out = String::new();
    let _ = s.read_to_string(&mut out);
    let code = out
        .split_whitespace()
        .nth(1)
        .and_then(|c| c.parse().ok())
        .unwrap_or(0);
    let payload = out.split("\r\n\r\n").nth(1).unwrap_or("");
    (
        code,
        serde_json::from_str(payload.trim()).unwrap_or(Value::Null),
    )
}

fn assert_signed(got: &Got) {
    let id = got.header("X-Tole-Delivery");
    assert_eq!(
        got.header("X-Tole-Signature"),
        webhook::sign(
            SIGN_KEY.as_bytes(),
            got.header("X-Tole-Timestamp"),
            id,
            Some(got.header("X-Tole-Event")),
            &got.body
        )
    );
    assert_eq!(id, got.json()["id"].as_str().unwrap());
}

#[test]
fn a_queued_write_and_its_denial_are_announced_signed_and_redacted() {
    let (port, rx) = receiver();
    let base = TestDir::new("tole-servewebhook-approval");
    std::fs::create_dir_all(base.join("home/tole")).unwrap();
    std::fs::create_dir_all(base.join("w")).unwrap();
    std::fs::write(
        base.join("home/tole/config.toml"),
        format!(
            "[webhook_out.gateway]\nurl = \"http://127.0.0.1:{port}/hook\"\n\
             hmac_key_env = \"{SIGN_VAR}\"\nallow_loopback = true\n\
             events = [\"approval_requested\", \"approval_resolved\"]\n"
        ),
    )
    .unwrap();
    let provider = mock_provider();
    let (child, sport) = common::spawn_listening(|p| {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_tole"));
        cmd.args(["serve", "--port", &p.to_string(), "--token", "t0ken"])
            .current_dir(base.join("w"))
            .env("CODECORA_HOME", base.join("home"))
            .env("TOLE_CRON_DIR", base.join("cron"))
            .env("TOLE_BASE_URL", &provider)
            .env("TOLE_MODEL", "mock-model")
            .env("TOLE_API_KEY", "sk-test-approval-hook")
            .env(SIGN_VAR, SIGN_KEY)
            .stdin(Stdio::null())
            .stdout(Stdio::null());
        cmd
    });
    struct Kill(std::process::Child);
    impl Drop for Kill {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
    let _guard = Kill(child);

    let (_, s) = http(sport, "POST", "/sessions", Some("{}"));
    let sid = s["sessionId"].as_str().unwrap().to_string();
    let (_, p) = http(
        sport,
        "POST",
        &format!("/sessions/{sid}/prompt"),
        Some(&json!({"text": "TOOL_MISSION write it"}).to_string()),
    );
    assert_eq!(p["stopReason"], "approval_required", "{p}");

    // The request is announced.
    let req = rx
        .recv_timeout(Duration::from_secs(30))
        .expect("approval_requested");
    assert_eq!(req.header("X-Tole-Event"), "approval_requested");
    assert_signed(&req);
    let ev = req.json();
    assert_eq!(ev["destination"], "gateway");
    assert_eq!(ev["data"]["tool"], "write_file");
    assert_eq!(ev["data"]["session_id"], sid);
    assert_eq!(ev["data"]["risk"], "write");
    let approval_id = ev["data"]["approval_id"].as_str().unwrap().to_string();
    // The key was in the tool call's path: it is redacted, and no path or
    // call fingerprint of the stored entry is in the event.
    let text = String::from_utf8_lossy(&req.body).to_string();
    assert!(!text.contains(SIGN_KEY), "the key leaked: {text}");
    assert!(
        ev["data"]["summary"].as_str().unwrap().contains("«"),
        "{text}"
    );
    assert!(ev["data"].get("fingerprint").is_none() && ev["data"].get("path").is_none());

    // The operator denies it: the decision is announced once.
    let (code, d) = http(
        sport,
        "POST",
        &format!("/approvals/{approval_id}/decision"),
        Some(&json!({"decision": "deny"}).to_string()),
    );
    assert!(code == 200 || code == 202, "{code} {d}");
    let res = rx
        .recv_timeout(Duration::from_secs(30))
        .expect("approval_resolved");
    assert_eq!(res.header("X-Tole-Event"), "approval_resolved");
    assert_signed(&res);
    let ev = res.json();
    assert_eq!(ev["data"]["approval_id"], approval_id.as_str());
    assert_eq!(ev["data"]["status"], "denied");
    assert!(!String::from_utf8_lossy(&res.body).contains(SIGN_KEY));
    assert!(
        rx.recv_timeout(Duration::from_secs(3)).is_err(),
        "exactly one request and one resolution"
    );
    assert!(
        !base.join(format!("w/{SIGN_KEY}.txt")).exists(),
        "a denied write must not run"
    );
}
