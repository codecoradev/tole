//! The scripted provider of the discussion tests (#1042, #1094).

use serde_json::{json, Value};

/// A provider for discussions: participants answer with text, the checker
/// meets its criteria citing the first entry it was shown, and a question that
/// says `hold` is answered only after `hold_ms`.
pub fn discussion_mock(hold_ms: u64) -> String {
    use std::io::{Read, Write};
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    std::thread::spawn(move || {
        for mut s in listener.incoming().flatten() {
            std::thread::spawn(move || {
                let mut buf = Vec::new();
                let mut chunk = [0u8; 65536];
                let body = loop {
                    let n = s.read(&mut chunk).unwrap_or(0);
                    buf.extend_from_slice(&chunk[..n]);
                    let split = buf.windows(4).position(|w| w == b"\r\n\r\n");
                    if let Some(v) =
                        split.and_then(|h| serde_json::from_slice::<Value>(&buf[h + 4..]).ok())
                    {
                        break v;
                    }
                    if n == 0 {
                        break Value::Null;
                    }
                };
                let msgs = body["messages"].as_array().cloned().unwrap_or_default();
                let text = |role: &str| {
                    let m = msgs.iter().find(|m| m["role"] == role);
                    m.and_then(|m| m["content"].as_str())
                        .unwrap_or("")
                        .to_string()
                };
                let (system, user) = (text("system"), text("user"));
                if user.contains("hold") {
                    std::thread::sleep(std::time::Duration::from_millis(hold_ms));
                }
                let reply = if system.contains("against its acceptance criteria") {
                    let e = user.lines().find_map(|l| l.strip_prefix("entry "));
                    let e = e.and_then(|l| l.split_whitespace().next()).unwrap_or("e1");
                    json!([{"met": true, "evidence": [e]}]).to_string()
                } else {
                    "use sqlite".to_string()
                };
                let data = json!({
                    "id": "c", "object": "chat.completion", "created": 1, "model": "mock",
                    "choices": [{"index": 0, "finish_reason": "stop",
                        "message": {"role": "assistant", "content": reply}}],
                    "usage": {"prompt_tokens": 120, "completion_tokens": 30, "total_tokens": 150}
                })
                .to_string();
                let head = "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nConnection: close";
                let _ = write!(s, "{head}\r\nContent-Length: {}\r\n\r\n{data}", data.len());
            });
        }
    });
    format!("http://{addr}/v1")
}
