//! Issue #178 — ACP `session/cancel` end-to-end with the REAL `tole acp`
//! binary: (1) a cancel that lands while the provider call is in flight
//! makes the prompt respond `stopReason: "cancelled"` and leaves the
//! session prompt-resumable; (2) a cancel that lands while a Write
//! permission request is pending unwinds the wait (fail closed) and the
//! turn settles `cancelled`, not `refusal`. Both leave a durable
//! `cancelled` record in the session JSONL.

use serde_json::{json, Value};
use std::io::{BufRead, Read, Write};
use std::net::TcpListener;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::Duration;

struct AcpProcess {
    child: Child,
    rx: mpsc::Receiver<String>,
    stdin: Arc<Mutex<std::process::ChildStdin>>,
    permission_count: Arc<AtomicUsize>,
}

impl AcpProcess {
    fn spawn(base_url: &str) -> Self {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_tole"));
        cmd.arg("acp")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .env_remove("TOLE_MEMORY")
            .env_remove("TOLE_MEMORY_NAMESPACE")
            .env_remove("TOLE_SYSTEM_PROMPT")
            .env_remove("TOLE_TRUST")
            .env_remove("TOLE_AGENT_DEPTH")
            .env_remove("TOLE_MODELS");
        cmd.env("TOLE_BASE_URL", base_url);
        cmd.env("TOLE_MODEL", "mock-model");
        cmd.env("TOLE_API_KEY", "sk-test");
        let mut child = cmd.spawn().expect("spawn tole acp");
        let stdout = child.stdout.take().expect("stdout piped");
        let stdin = Arc::new(Mutex::new(child.stdin.take().expect("stdin piped")));
        let permission_count = Arc::new(AtomicUsize::new(0));
        let count = Arc::clone(&permission_count);
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let reader = std::io::BufReader::new(stdout);
            for line in reader.lines().map_while(Result::ok) {
                let Ok(msg) = serde_json::from_str::<Value>(&line) else {
                    continue;
                };
                // Count permission requests but NEVER answer them —
                // scenario B needs one to stay pending.
                if msg.get("method").and_then(Value::as_str) == Some("session/request_permission") {
                    count.fetch_add(1, Ordering::SeqCst);
                }
                if tx.send(line).is_err() {
                    break;
                }
            }
        });
        Self {
            child,
            rx,
            stdin,
            permission_count,
        }
    }

    fn send(&self, v: &Value) {
        let mut w = self.stdin.lock().unwrap();
        writeln!(w, "{v}").unwrap();
        w.flush().unwrap();
    }

    /// Scan the stream for the response with `id` (skipping any other
    /// lines, e.g. session/update notifications).
    fn wait_response(&self, id: u64, timeout: Duration) -> Value {
        let deadline = std::time::Instant::now() + timeout;
        let mut seen: Vec<String> = Vec::new();
        while std::time::Instant::now() < deadline {
            let Ok(line) = self
                .rx
                .recv_timeout(deadline.saturating_duration_since(std::time::Instant::now()))
            else {
                break;
            };
            let Ok(msg) = serde_json::from_str::<Value>(&line) else {
                continue;
            };
            if msg.get("id").and_then(Value::as_u64) == Some(id) && msg.get("method").is_none() {
                return msg;
            }
            seen.push(line);
        }
        panic!(
            "no response for id {id} within {timeout:?}; saw: {}",
            seen.join("\n")
        );
    }

    /// Scan for the FIRST notification with `method`.
    fn wait_notification(&self, method: &str, timeout: Duration) -> Value {
        let deadline = std::time::Instant::now() + timeout;
        while std::time::Instant::now() < deadline {
            let Ok(line) = self
                .rx
                .recv_timeout(deadline.saturating_duration_since(std::time::Instant::now()))
            else {
                break;
            };
            let Ok(msg) = serde_json::from_str::<Value>(&line) else {
                continue;
            };
            if msg.get("method").and_then(Value::as_str) == Some(method) {
                return msg;
            }
        }
        panic!("no {method} notification within {timeout:?}");
    }
}

impl Drop for AcpProcess {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Mock provider: a `DELAYED` prompt gets its answer after a 2 s sleep
/// (so the cancel can land mid-provider-call); anything else — or any
/// step after a tool result — answers immediately with a Final.
/// `TOOL_MISSION` asks for a write_file tool call instead.
fn spawn_mock() -> String {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            std::thread::spawn(move || {
                let mut s = stream;
                let mut buf = Vec::new();
                let mut chunk = [0u8; 16384];
                let mut body = Value::Null;
                let deadline = std::time::Instant::now() + Duration::from_secs(10);
                while std::time::Instant::now() < deadline {
                    let n = s.read(&mut chunk).unwrap_or(0);
                    if n > 0 {
                        buf.extend_from_slice(&chunk[..n]);
                    }
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
                let msgs = body["messages"].as_array().cloned().unwrap_or_default();
                let last_user = msgs
                    .iter()
                    .rev()
                    .find(|m| m["role"] == "user")
                    .and_then(|m| m["content"].as_str())
                    .unwrap_or("")
                    .to_string();
                let has_tool_result = match msgs.iter().rposition(|m| m["role"] == "user") {
                    Some(i) => msgs[i + 1..].iter().any(|m| m["role"] == "tool"),
                    None => false,
                };
                let reply = if !has_tool_result && last_user.contains("TOOL_MISSION") {
                    json!({
                        "id": "chatcmpl-mock", "object": "chat.completion", "created": 1,
                        "model": "mock",
                        "choices": [{"index": 0, "message": {"role": "assistant",
                            "content": null,
                            "tool_calls": [{"id": "call_1", "type": "function",
                                "function": {"name": "write_file", "arguments":
                                    "{\"path\":\"canceled.txt\",\"content\":\"x\"}"}}]},
                            "finish_reason": "tool_calls"}],
                        "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2}
                    })
                } else {
                    if !has_tool_result && last_user.contains("DELAYED") {
                        std::thread::sleep(Duration::from_secs(2));
                    }
                    json!({
                        "id": "chatcmpl-mock", "object": "chat.completion", "created": 1,
                        "model": "mock",
                        "choices": [{"index": 0, "message": {"role": "assistant",
                            "content": "MISSION_DONE"}, "finish_reason": "stop"}],
                        "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2}
                    })
                };
                let data = reply.to_string();
                let resp = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    data.len(),
                    data
                );
                let _ = s.write_all(resp.as_bytes());
            });
        }
    });
    format!("http://{addr}/v1")
}

fn tmpdir(tag: &str) -> std::path::PathBuf {
    let d = std::env::temp_dir().join(format!(
        "tole-acp-cancel-{tag}-{}-{:x}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis(),
        std::process::id()
    ));
    std::fs::create_dir_all(&d).unwrap();
    d
}

/// Scenario A: cancel while the provider call is in flight → the prompt
/// responds `cancelled`; the session JSONL carries the durable record;
/// a follow-up prompt on the SAME session completes normally.
#[test]
fn acp_cancel_mid_provider_settles_cancelled_and_resumable() {
    let base = spawn_mock();
    let cwd = tmpdir("mid-provider");
    let acp = AcpProcess::spawn(&base);

    acp.send(&json!({
        "jsonrpc": "2.0", "id": 1, "method": "initialize",
        "params": {"protocolVersion": 1, "clientCapabilities": {}}
    }));
    acp.wait_response(1, Duration::from_secs(20));
    acp.send(&json!({
        "jsonrpc": "2.0", "id": 2, "method": "session/new",
        "params": {"cwd": cwd.to_string_lossy()}
    }));
    let created = acp.wait_response(2, Duration::from_secs(20));
    let session_id = created["result"]["sessionId"].as_str().unwrap().to_string();

    // Long turn: the provider sleeps 2 s before answering.
    acp.send(&json!({
        "jsonrpc": "2.0", "id": 10, "method": "session/prompt",
        "params": {"sessionId": session_id, "prompt": [{"type": "text", "text": "DELAYED mission"}]}
    }));
    // Let the turn enter the provider call, then cancel.
    std::thread::sleep(Duration::from_millis(500));
    acp.send(&json!({
        "jsonrpc": "2.0", "method": "session/cancel",
        "params": {"sessionId": session_id}
    }));
    let stopped = acp.wait_response(10, Duration::from_secs(30));
    assert_eq!(
        stopped["result"]["stopReason"].as_str(),
        Some("cancelled"),
        "got {stopped}"
    );

    // Durable record in the session JSONL.
    let mut found_session = None;
    let dirs = std::fs::read_dir(cwd.join(".tole/sessions")).unwrap();
    for e in dirs.flatten() {
        if e.file_name().to_string_lossy().contains(&session_id) {
            found_session = Some(e.path());
        }
    }
    let jsonl = std::fs::read_to_string(found_session.expect("session file")).unwrap();
    assert!(jsonl.contains("cancelled"), "durable record missing");

    // The SAME session is prompt-resumable: a fast turn completes.
    acp.send(&json!({
        "jsonrpc": "2.0", "id": 11, "method": "session/prompt",
        "params": {"sessionId": session_id, "prompt": [{"type": "text", "text": "quick"}]}
    }));
    let resumed = acp.wait_response(11, Duration::from_secs(30));
    assert_eq!(resumed["result"]["stopReason"].as_str(), Some("end_turn"));
}

/// Scenario B: cancel while a Write permission request is pending → the
/// wait unwinds fail-closed, the prompt settles `cancelled` (not
/// `refusal`), and the session is reusable.
#[test]
fn acp_cancel_during_pending_permission_settles_cancelled() {
    let base = spawn_mock();
    let cwd = tmpdir("pending-permission");
    let acp = AcpProcess::spawn(&base);

    acp.send(&json!({
        "jsonrpc": "2.0", "id": 1, "method": "initialize",
        "params": {"protocolVersion": 1, "clientCapabilities": {}}
    }));
    acp.wait_response(1, Duration::from_secs(20));
    acp.send(&json!({
        "jsonrpc": "2.0", "id": 2, "method": "session/new",
        "params": {"cwd": cwd.to_string_lossy()}
    }));
    let created = acp.wait_response(2, Duration::from_secs(20));
    let session_id = created["result"]["sessionId"].as_str().unwrap().to_string();

    // Turn asks for a Write → permission request stays pending.
    acp.send(&json!({
        "jsonrpc": "2.0", "id": 10, "method": "session/prompt",
        "params": {"sessionId": session_id, "prompt": [{"type": "text", "text": "TOOL_MISSION"}]}
    }));
    acp.wait_notification("session/request_permission", Duration::from_secs(30));
    // Cancel with the permission still pending: the wait must unwind
    // (≤ ~1 s slice) and the turn settle cancelled.
    let t0 = std::time::Instant::now();
    acp.send(&json!({
        "jsonrpc": "2.0", "method": "session/cancel",
        "params": {"sessionId": session_id}
    }));
    let stopped = acp.wait_response(10, Duration::from_secs(30));
    assert_eq!(
        stopped["result"]["stopReason"].as_str(),
        Some("cancelled"),
        "got {stopped}"
    );
    assert!(
        t0.elapsed() < Duration::from_secs(20),
        "cancel must unwind the pending permission quickly, took {:?}",
        t0.elapsed()
    );
    assert_eq!(acp.permission_count.load(Ordering::SeqCst), 1);

    // The cancelled turn wrote NOTHING (the effect was gated).
    let victim = cwd.join("canceled.txt");
    assert!(!victim.exists(), "cancelled tool must not execute");

    // Session reusable.
    acp.send(&json!({
        "jsonrpc": "2.0", "id": 11, "method": "session/prompt",
        "params": {"sessionId": session_id, "prompt": [{"type": "text", "text": "quick"}]}
    }));
    let resumed = acp.wait_response(11, Duration::from_secs(30));
    assert_eq!(resumed["result"]["stopReason"].as_str(), Some("end_turn"));
}
