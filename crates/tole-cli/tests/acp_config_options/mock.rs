//! The mock provider: records the model field of every request, always finals.

use crate::sse::sse_response;
use serde_json::{json, Value};
use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// `models_reply`: what GET /models answers — a fixed two-id list (the
/// default), a 500, or an empty list. Issue #195 E2E coverage.
#[derive(Clone, Copy)]
pub enum ModelsReply {
    List,
    ServerError,
    Empty,
    /// Accept the request and never answer (a hanging gateway, #319).
    Hang,
}

pub fn spawn_mock() -> (
    String,
    Arc<Mutex<Vec<String>>>,
    Arc<std::sync::atomic::AtomicUsize>,
) {
    spawn_mock_with(ModelsReply::List)
}

pub fn spawn_mock_with(
    models_reply: ModelsReply,
) -> (
    String,
    Arc<Mutex<Vec<String>>>,
    Arc<std::sync::atomic::AtomicUsize>,
) {
    use std::sync::atomic::{AtomicUsize, Ordering};
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let seen: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let models_hits = Arc::new(AtomicUsize::new(0));
    let seen_inner = Arc::clone(&seen);
    let hits_inner = Arc::clone(&models_hits);
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let seen_conn = Arc::clone(&seen_inner);
            let hits_conn = Arc::clone(&hits_inner);
            std::thread::spawn(move || {
                let mut s = stream;
                let mut buf = Vec::new();
                let mut chunk = [0u8; 16384];
                let mut body = Value::Null;
                let mut streamed = false;
                let mut is_models_get = false;
                let deadline = std::time::Instant::now() + Duration::from_secs(10);
                while std::time::Instant::now() < deadline {
                    let n = s.read(&mut chunk).unwrap_or(0);
                    if n > 0 {
                        buf.extend_from_slice(&chunk[..n]);
                    }
                    if let Some(h) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                        if !is_models_get {
                            let head = String::from_utf8_lossy(&buf[..h]).to_string();
                            let request_line = head.lines().next().unwrap_or("");
                            is_models_get = request_line.starts_with("GET ")
                                && request_line.contains("/models");
                        }
                        if is_models_get {
                            break;
                        }
                        // POST: keep reading until the body parses — it
                        // may arrive in segments after the header block.
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
                if is_models_get {
                    hits_conn.fetch_add(1, Ordering::SeqCst);
                    if matches!(models_reply, ModelsReply::Hang) {
                        std::thread::sleep(Duration::from_secs(45));
                        return;
                    }
                    let (status, payload) = match models_reply {
                        ModelsReply::List => (
                            "200 OK",
                            json!({"data": [{"id": "model-b"}, {"id": "model-a"}]}),
                        ),
                        ModelsReply::Empty => ("200 OK", json!({"data": []})),
                        ModelsReply::Hang => unreachable!("handled above"),
                        ModelsReply::ServerError => {
                            ("500 Internal Server Error", json!({"error": "down"}))
                        }
                    };
                    let data = payload.to_string();
                    let resp = format!(
                        "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                        data.len(),
                        data
                    );
                    let _ = s.write_all(resp.as_bytes());
                    return;
                }
                seen_conn
                    .lock()
                    .unwrap()
                    .push(body["model"].as_str().unwrap_or("<none>").to_string());
                let reply = json!({
                    "id": "chatcmpl-mock", "object": "chat.completion", "created": 1,
                    "model": body["model"].as_str().unwrap_or("mock"),
                    "choices": [{
                        "index": 0,
                        "message": {"role": "assistant", "content": "PICKER_OK"},
                        "finish_reason": "stop"
                    }],
                    "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2}
                });
                let resp = sse_response(&reply, streamed);
                let _ = s.write_all(resp.as_bytes());
            });
        }
    });
    (format!("http://{addr}/v1"), seen, models_hits)
}
