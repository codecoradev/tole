//! #200 remote approvals — end-to-end over the REAL serve face: a
//! non-preauthorized Write queues a pending decision, the turn settles
//! resumably, `allow` triggers the approvals-only resume (the work
//! executes), `deny` records the verdict, and the audit register lands.

// These tests spawn the real `tole acp`/`tole serve` binary, which only exists
// with the `shell-tools` feature (#349): the bare profile has no such face.
#![cfg(feature = "shell-tools")]

use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

struct ServeProcess {
    child: Child,
    port: u16,
}

impl ServeProcess {
    fn spawn(env: &[(&str, &str)], token: &str, port: u16) -> Self {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_tole"));
        cmd.args(["serve", "--port", &port.to_string(), "--token", token])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::inherit());
        for (k, v) in env {
            cmd.env(k, v);
        }
        let child = cmd.spawn().expect("spawn tole serve");
        let me = Self { child, port };
        // Health-poll until the listener is up.
        for _ in 0..50 {
            if me.http("GET", "/health", None).0 == 200 {
                return me;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        panic!("serve never became healthy");
    }

    fn http(&self, method: &str, path: &str, body: Option<&str>) -> (u16, Value) {
        // Connect failures read as code 0 — the health poll retries; a
        // mid-test refusal surfaces as a failed assertion, not a panic.
        let mut s = match std::net::TcpStream::connect(("127.0.0.1", self.port)) {
            Ok(s) => s,
            Err(_) => return (0, Value::Null),
        };
        let req = match body {
            Some(b) => format!(
                "{method} {path} HTTP/1.1\r\nAuthorization: Bearer t0ken\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{b}",
                b.len()
            ),
            None => format!(
                "{method} {path} HTTP/1.1\r\nAuthorization: Bearer t0ken\r\nConnection: close\r\n\r\n"
            ),
        };
        s.write_all(req.as_bytes()).unwrap();
        let mut reader = BufReader::new(s);
        let mut status_line = String::new();
        reader.read_line(&mut status_line).unwrap();
        let code = status_line
            .split_whitespace()
            .nth(1)
            .and_then(|c| c.parse::<u16>().ok())
            .unwrap_or(0);
        let mut payload = String::new();
        loop {
            let mut line = String::new();
            if reader.read_line(&mut line).unwrap() == 0 {
                break;
            }
            if line == "\r\n" || line == "\n" {
                reader.read_to_string(&mut payload).unwrap();
                break;
            }
        }
        (
            code,
            serde_json::from_str(payload.trim()).unwrap_or(Value::Null),
        )
    }
}

impl Drop for ServeProcess {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Mock: TOOL_MISSION → write_file; after the tool result → final.
fn spawn_mock() -> String {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let hits = Arc::new(AtomicUsize::new(0));
    let h2 = Arc::clone(&hits);
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let h = Arc::clone(&h2);
            std::thread::spawn(move || {
                let mut s = stream;
                let mut buf = Vec::new();
                let mut chunk = [0u8; 16384];
                let mut body = Value::Null;
                let mut streamed = false;
                let deadline = std::time::Instant::now() + Duration::from_secs(10);
                while std::time::Instant::now() < deadline {
                    let n = s.read(&mut chunk).unwrap_or(0);
                    if n > 0 {
                        buf.extend_from_slice(&chunk[..n]);
                    }
                    if let Some(hpos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                        if let Ok(v) = serde_json::from_slice::<Value>(&buf[hpos + 4..]) {
                            streamed = v.get("stream").and_then(Value::as_bool) == Some(true);
                            body = v;
                            break;
                        }
                    }
                    if n == 0 {
                        break;
                    }
                }
                let _ = h.load(Ordering::SeqCst);
                let msgs = body["messages"].as_array().cloned().unwrap_or_default();
                let has_tool_result = match msgs.iter().rposition(|m| m["role"] == "user") {
                    Some(i) => msgs[i + 1..].iter().any(|m| m["role"] == "tool"),
                    None => false,
                };
                let reply = if !has_tool_result {
                    json!({
                        "id": "c", "object": "chat.completion", "created": 1, "model": "mock",
                        "choices": [{"index": 0, "message": {"role": "assistant",
                            "content": null,
                            "tool_calls": [{"id": "call_1", "type": "function",
                                "function": {"name": "write_file", "arguments":
                                    "{\"path\":\"approved.txt\",\"content\":\"remote ok\"}"}}]},
                            "finish_reason": "tool_calls"}],
                        "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2}
                    })
                } else {
                    json!({
                        "id": "c", "object": "chat.completion", "created": 1, "model": "mock",
                        "choices": [{"index": 0, "message": {"role": "assistant",
                            "content": "APPROVED_WORK_DONE"}, "finish_reason": "stop"}],
                        "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2}
                    })
                };
                let resp = if streamed {
                    let msg = &reply["choices"][0]["message"];
                    let mut out = String::new();
                    if let Some(c) = msg["content"].as_str() {
                        out.push_str(&format!(
                            "data: {}\n\n",
                            json!({"choices": [{"index": 0,
                                "delta": {"content": c}, "finish_reason": null}]})
                        ));
                    }
                    if let Some(tcs) = msg["tool_calls"].as_array() {
                        for (i, tc) in tcs.iter().enumerate() {
                            out.push_str(&format!(
                                "data: {}\n\n",
                                json!({"choices": [{"index": 0, "delta": {"tool_calls": [
                                    {"index": i, "id": tc["id"], "function": {
                                        "name": tc["function"]["name"],
                                        "arguments": tc["function"]["arguments"]}}]},
                                    "finish_reason": null}]})
                            ));
                        }
                    }
                    out.push_str(&format!(
                        "data: {}\n\n",
                        json!({"choices": [], "usage": {"prompt_tokens": 1,
                               "completion_tokens": 1, "total_tokens": 2}})
                    ));
                    out.push_str("data: [DONE]\n\n");
                    format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                        out.len(), out
                    )
                } else {
                    let data = reply.to_string();
                    format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                        data.len(), data
                    )
                };
                let _ = s.write_all(resp.as_bytes());
            });
        }
    });
    format!("http://{addr}/v1")
}

fn temp_dir(tag: &str) -> std::path::PathBuf {
    let d = std::env::temp_dir().join(format!(
        "tole-approvals-{tag}-{}-{:x}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

/// The full round trip: queue → allow → work executes → audit register.
#[test]
fn approval_allow_round_trip_executes_the_work() {
    // Issue #259: clear any stale marker BEFORE the run, not just after
    // the assert — a crashed prior run must not false-pass this test.
    let _ = std::fs::remove_file(std::path::Path::new("approved.txt"));
    let base = spawn_mock();
    let _dir = temp_dir("allow");
    let env = [
        ("TOLE_BASE_URL".to_string(), base),
        ("TOLE_MODEL".to_string(), "mock-model".to_string()),
        ("TOLE_API_KEY".to_string(), "sk-test".to_string()),
    ];
    let env_ref: Vec<(&str, &str)> = env.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
    let serve = ServeProcess::spawn(&env_ref, "t0ken", 47901);

    // Create a session + run the write mission: no --allow → the Write
    // queues and the turn settles (fail-closed).
    let (_, body) = serve.http("POST", "/sessions", Some("{}"));
    let sid = body["sessionId"].as_str().unwrap().to_string();
    let (_, prompt_body) = serve.http(
        "POST",
        &format!("/sessions/{sid}/prompt"),
        Some(&json!({"text": "TOOL_MISSION write it"}).to_string()),
    );
    assert_eq!(prompt_body["stopReason"], "refusal", "fail-closed settle");

    // The queue shows the pending decision.
    let (code, list) = serve.http("GET", "/approvals", None);
    assert_eq!(code, 200);
    let approvals = list["approvals"].as_array().unwrap();
    assert_eq!(approvals.len(), 1, "{list}");
    assert_eq!(approvals[0]["tool"], "write_file");
    assert_eq!(approvals[0]["status"], "pending");
    assert_eq!(approvals[0]["sessionId"], sid);
    let approval_id = approvals[0]["id"].as_str().unwrap().to_string();

    // ALLOW: 202 + async approvals-only resume → the work executes.
    let (code, decision) = serve.http(
        "POST",
        &format!("/approvals/{approval_id}/decision"),
        Some(&json!({"decision": "allow"}).to_string()),
    );
    assert_eq!(code, 202, "{decision}");
    assert_eq!(decision["resuming"], true);

    // Wait for the resumed turn to settle.
    for _ in 0..50 {
        let (_, st) = serve.http("GET", &format!("/sessions/{sid}/status"), None);
        if st["busy"] != json!(true) {
            break;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    // The queue entry is approved; the work landed.
    let (_, list) = serve.http("GET", "/approvals", None);
    assert_eq!(list["approvals"][0]["status"], "approved", "{list}");
    // The one-shot replay actually EXECUTED the write: the tool is
    // jailed to the serve process cwd (the test binary's package dir).
    // Issue #259: the stale-marker cleanup happens BEFORE the run (top
    // of this test), so this assert proves THIS run's write landed.
    let work_file = std::path::Path::new("approved.txt");
    assert!(work_file.exists(), "the approved write must have executed");
    let _ = std::fs::remove_file(work_file);
}

/// DENY: the verdict is recorded, nothing executes, the queue reflects it.
#[test]
fn approval_deny_records_verdict_without_executing() {
    let base = spawn_mock();
    let dir = temp_dir("deny");
    let env = [
        ("TOLE_BASE_URL".to_string(), base),
        ("TOLE_MODEL".to_string(), "mock-model".to_string()),
        ("TOLE_API_KEY".to_string(), "sk-test".to_string()),
    ];
    let env_ref: Vec<(&str, &str)> = env.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
    let serve = ServeProcess::spawn(&env_ref, "t0ken", 47903);

    let (_, body) = serve.http("POST", "/sessions", Some("{}"));
    let sid = body["sessionId"].as_str().unwrap().to_string();
    let _ = serve.http(
        "POST",
        &format!("/sessions/{sid}/prompt"),
        Some(&json!({"text": "TOOL_MISSION write it"}).to_string()),
    );
    let (_, list) = serve.http("GET", "/approvals", None);
    let approval_id = list["approvals"][0]["id"].as_str().unwrap().to_string();

    let (code, decision) = serve.http(
        "POST",
        &format!("/approvals/{approval_id}/decision"),
        Some(&json!({"decision": "deny"}).to_string()),
    );
    assert_eq!(code, 200, "{decision}");
    assert_eq!(decision["status"], "denied");

    let (_, list) = serve.http("GET", "/approvals", None);
    assert_eq!(list["approvals"][0]["status"], "denied");
    let _ = dir;
}

/// The CLI consumer (`tole approvals list`) speaks to the same queue.
#[test]
fn cli_consumer_lists_the_queue() {
    let base = spawn_mock();
    let _dir = temp_dir("cli");
    let env = [
        ("TOLE_BASE_URL".to_string(), base),
        ("TOLE_MODEL".to_string(), "mock-model".to_string()),
        ("TOLE_API_KEY".to_string(), "sk-test".to_string()),
    ];
    let env_ref: Vec<(&str, &str)> = env.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
    let serve = ServeProcess::spawn(&env_ref, "t0ken", 47905);
    let _ = serve.http("POST", "/sessions", Some("{}"));
    let _ = serve.http(
        "POST",
        "/sessions/PLACEHOLDER/prompt",
        Some(&json!({"text": "x"}).to_string()),
    );
    // Create a real pending entry through a real session.
    let (_, body) = serve.http("POST", "/sessions", Some("{}"));
    let sid = body["sessionId"].as_str().unwrap().to_string();
    let _ = serve.http(
        "POST",
        &format!("/sessions/{sid}/prompt"),
        Some(&json!({"text": "TOOL_MISSION write"}).to_string()),
    );

    let mut cmd = Command::new(env!("CARGO_BIN_EXE_tole"));
    cmd.args([
        "approvals",
        "list",
        "--url",
        &format!("http://127.0.0.1:{}", serve.port),
        "--token",
        "t0ken",
    ])
    .env_remove("TOLE_MEMORY")
    .env_remove("TOLE_TRUST");
    let out = cmd.output().expect("run tole approvals");
    let stdout = String::from_utf8_lossy(&out.stdout).to_string();
    assert!(
        out.status.success(),
        "cli failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(stdout.contains("write_file"), "{stdout}");
    assert!(stdout.contains("pending"), "{stdout}");
}
