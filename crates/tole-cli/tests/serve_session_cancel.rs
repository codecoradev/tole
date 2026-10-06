//! Issue #178 follow-up (owner directive: serve follows the ACP design)
//! — REST `POST /sessions/{id}/cancel` end-to-end with the REAL `tole
//! serve` binary: a cancel that lands while a provider call is in
//! flight makes the blocked prompt respond `stopReason: "cancelled"`,
//! and the session accepts a follow-up prompt normally. Unknown
//! sessions 404.

use serde_json::{json, Value};
use std::io::{Read, Write};
use std::net::TcpListener;
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::time::Duration;

struct ServeProcess {
    child: Child,
    addr: String,
    token: String,
}

impl ServeProcess {
    fn spawn(base_url: &str) -> Arc<Self> {
        let port_listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = port_listener.local_addr().unwrap().port();
        drop(port_listener);
        let cwd = std::env::temp_dir().join(format!(
            "tole-serve-cancel-{}-{:x}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_millis(),
            std::process::id()
        ));
        std::fs::create_dir_all(&cwd).unwrap();
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_tole"));
        cmd.args([
            "serve",
            "--port",
            &port.to_string(),
            "--workspace",
            cwd.to_string_lossy().as_ref(),
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .env_remove("TOLE_MEMORY")
        .env_remove("TOLE_MEMORY_NAMESPACE")
        .env_remove("TOLE_TRUST");
        cmd.env("TOLE_BASE_URL", base_url);
        cmd.env("TOLE_MODEL", "mock-model");
        cmd.env("TOLE_API_KEY", "sk-test");
        let token = "test-token-123".to_string();
        cmd.env("TOLE_SERVE_TOKEN", &token);
        let child = cmd.spawn().expect("spawn tole serve");
        let proc = Arc::new(Self {
            child,
            addr: format!("127.0.0.1:{port}"),
            token,
        });
        let deadline = std::time::Instant::now() + Duration::from_secs(20);
        while std::time::Instant::now() < deadline {
            if let Some((200, _)) = proc.request("GET", "/health", "") {
                return proc;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        panic!("serve did not become healthy");
    }

    /// One HTTP request; returns (status, parsed JSON body). Handles
    /// both identity and chunked response framing (dechunked naively).
    fn request(&self, method: &str, path: &str, body: &str) -> Option<(u16, Value)> {
        let mut stream = std::net::TcpStream::connect(&self.addr).ok()?;
        let req = format!(
            "{method} {path} HTTP/1.1\r\nHost: localhost\r\nAuthorization: Bearer {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            self.token,
            body.len()
        );
        stream.write_all(req.as_bytes()).ok()?;
        stream
            .set_read_timeout(Some(Duration::from_secs(60)))
            .ok()?;
        let mut buf = Vec::new();
        let mut chunk = [0u8; 8192];
        loop {
            let n = stream.read(&mut chunk).unwrap_or(0);
            if n == 0 {
                break;
            }
            buf.extend_from_slice(&chunk[..n]);
        }
        let text = String::from_utf8_lossy(&buf);
        let code: u16 = text.split_whitespace().nth(1)?.parse().ok()?;
        let raw = text.split("\r\n\r\n").nth(1).unwrap_or("");
        let chunked = text.to_lowercase().contains("transfer-encoding: chunked");
        let payload = if chunked {
            // Naive dechunk: drop the hex-size lines, keep payload lines.
            let mut out = String::new();
            for line in raw.split("\r\n") {
                let is_size = !line.is_empty()
                    && line.len() <= 8
                    && line.chars().all(|c| c.is_ascii_hexdigit());
                if !is_size {
                    out.push_str(line);
                }
            }
            out
        } else {
            raw.to_string()
        };
        let v: Value = serde_json::from_str(payload.trim()).unwrap_or(Value::Null);
        Some((code, v))
    }
}

impl Drop for ServeProcess {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

// Mock provider: a `DELAYED` prompt answers after 2 s; anything else
// answers immediately (fast follow-up turn).
//
// Convert a plain chat-completion reply into an SSE stream body
// (issue #196 phase 3 mocks).

fn completion_to_sse(reply: &Value) -> String {
    let mut out = String::new();
    let msg = &reply["choices"][0]["message"];
    let model = reply["model"].as_str().unwrap_or("mock");
    let mut chunk_of = |delta: Value| {
        let c = json!({
            "id": "chatcmpl-sse", "object": "chat.completion.chunk", "created": 1,
            "model": model,
            "choices": [{"index": 0, "delta": delta, "finish_reason": null}],
            "usage": null
        });
        out.push_str(&format!("data: {c}\n\n"));
    };
    if let Some(c) = msg["content"].as_str() {
        if !c.is_empty() {
            let (a, b) = c.split_at(c.len().div_ceil(2));
            chunk_of(json!({"content": a}));
            chunk_of(json!({"content": b}));
        }
    }
    if let Some(tcs) = msg["tool_calls"].as_array() {
        for (i, tc) in tcs.iter().enumerate() {
            let name = tc["function"]["name"].as_str().unwrap_or("");
            let args = tc["function"]["arguments"].as_str().unwrap_or("{}");
            let (a1, a2) = args.split_at(args.len().div_ceil(2));
            chunk_of(json!({"tool_calls": [{"index": i, "id": tc["id"],
                "function": {"name": name, "arguments": a1}}]}));
            chunk_of(json!({"tool_calls": [{"index": i,
                "function": {"arguments": a2}}]}));
        }
    }
    if !msg["tool_calls"].is_null() {
        out.push_str(&format!(
            "data: {}\n\n",
            json!({"choices": [{"index": 0, "delta": {}, "finish_reason": "tool_calls"}], "usage": null})
        ));
    }
    out.push_str(&format!(
        "data: {}\n\n",
        json!({"choices": [], "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2}})
    ));
    out.push_str("data: [DONE]\n\n");
    out
}

fn sse_response(reply: &Value, streamed: bool) -> String {
    if streamed {
        let body = completion_to_sse(reply);
        format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            body.len(),
            body
        )
    } else {
        let data = reply.to_string();
        format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            data.len(),
            data
        )
    }
}

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
                let mut streamed = false;
                let deadline = std::time::Instant::now() + Duration::from_secs(10);
                while std::time::Instant::now() < deadline {
                    let n = s.read(&mut chunk).unwrap_or(0);
                    if n > 0 {
                        buf.extend_from_slice(&chunk[..n]);
                    }
                    if let Some(h) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                        if let Ok(v) = serde_json::from_slice::<Value>(&buf[h + 4..]) {
                            streamed = v.get("stream").and_then(Value::as_bool) == Some(true);
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
                if last_user.contains("DELAYED") {
                    std::thread::sleep(Duration::from_secs(2));
                }
                let reply = json!({
                    "id": "chatcmpl-mock", "object": "chat.completion", "created": 1,
                    "model": "mock",
                    "choices": [{"index": 0, "message": {"role": "assistant",
                        "content": "DONE"}, "finish_reason": "stop"}],
                    "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2}
                });
                let resp = sse_response(&reply, streamed);
                let _ = s.write_all(resp.as_bytes());
            });
        }
    });
    format!("http://{addr}/v1")
}

#[test]
fn serve_cancel_mid_provider_settles_cancelled_and_resumable() {
    let base = spawn_mock();
    let serve = ServeProcess::spawn(&base);

    // Create a session (workspace defaults to the server cwd).
    let (code, created) = serve
        .request("POST", "/sessions", "{}")
        .expect("create session");
    assert_eq!(code, 200, "{created}");
    let sid = created["sessionId"].as_str().unwrap().to_string();

    // Unknown session: 404.
    let (code, _) = serve.request("POST", "/sessions/nope/cancel", "").unwrap();
    assert_eq!(code, 404);

    // Start the DELAYED prompt on a background thread; it blocks for
    // ~2 s at the provider.
    let thread_serve = Arc::clone(&serve);
    let sid_clone = sid.clone();
    let prompt_thread = std::thread::spawn(move || {
        thread_serve.request(
            "POST",
            &format!("/sessions/{sid_clone}/prompt"),
            r#"{"text": "DELAYED mission"}"#,
        )
    });
    // Cancel while the provider call is in flight.
    std::thread::sleep(Duration::from_millis(500));
    let (code, ack) = serve
        .request("POST", &format!("/sessions/{sid}/cancel"), "")
        .expect("cancel");
    assert_eq!(code, 200, "{ack}");
    assert_eq!(ack["cancelled"], json!(true));

    // The blocked prompt settles "cancelled".
    let (pcode, presp) = prompt_thread.join().unwrap().expect("prompt response");
    assert_eq!(pcode, 200, "{presp}");
    assert_eq!(presp["stopReason"].as_str(), Some("cancelled"), "{presp}");

    // Follow-up prompt on the SAME session works normally.
    let (code, resp) = serve
        .request(
            "POST",
            &format!("/sessions/{sid}/prompt"),
            r#"{"text": "quick"}"#,
        )
        .expect("follow-up");
    assert_eq!(code, 200, "{resp}");
    assert_eq!(resp["stopReason"].as_str(), Some("end_turn"), "{resp}");
}
