//! The scripted mock provider of the TUI e2e tests.

use serde_json::{json, Value};
use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
/// Reply by the last message: a tool result -> "WROTE-OK"; a prompt with
/// "write" -> a write_file call; "evil" -> an answer carrying ESC/CR; "slow" -> silence for 20s; else a two-chunk
/// streamed answer. Always SSE when the request streams.
pub fn spawn_mock() -> String {
    spawn_mock_recording(Arc::default())
}

/// Every chat request body the mock received, in order.
pub type Bodies = Arc<Mutex<Vec<Value>>>;

pub fn spawn_mock_recording(bodies: Bodies) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let bodies = bodies.clone();
            std::thread::spawn(move || serve(stream, bodies));
        }
    });
    format!("http://{addr}/v1")
}

fn serve(mut s: std::net::TcpStream, bodies: Bodies) {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 65536];
    let deadline = Instant::now() + Duration::from_secs(10);
    let (head_end, len) = loop {
        let n = s.read(&mut chunk).unwrap_or(0);
        buf.extend_from_slice(&chunk[..n]);
        if let Some(h) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            let head = String::from_utf8_lossy(&buf[..h]).to_lowercase();
            let len = head
                .lines()
                .find_map(|l| l.strip_prefix("content-length:"))
                .and_then(|v| v.trim().parse::<usize>().ok())
                .unwrap_or(0);
            break (h + 4, len);
        }
        if n == 0 || Instant::now() > deadline {
            return;
        }
    };
    while buf.len() < head_end + len {
        let n = s.read(&mut chunk).unwrap_or(0);
        if n == 0 {
            return;
        }
        buf.extend_from_slice(&chunk[..n]);
    }
    let path_is_models = buf.starts_with(b"GET");
    let body: Value = serde_json::from_slice(&buf[head_end..head_end + len]).unwrap_or(Value::Null);
    let mut reply = |ctype: &str, data: String| {
        let r = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: {ctype}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{data}",
            data.len()
        );
        let _ = s.write_all(r.as_bytes());
    };
    if path_is_models {
        return reply(
            "application/json",
            json!({"data": [{"id": "m"}, {"id": "alt-model"}, {"id": "other-one"}]}).to_string(),
        );
    }
    bodies.lock().unwrap().push(body.clone());
    let last = body["messages"]
        .as_array()
        .and_then(|m| m.last())
        .cloned()
        .unwrap_or(Value::Null);
    let said = last["content"].as_str().unwrap_or("").to_string();
    let call = json!({"index": 0, "id": "c1", "function": {"name": "write_file",
        "arguments": json!({"path": "out.txt", "content": "x"}).to_string()}});
    let deltas: Vec<Value> = if last["role"] == "tool" {
        vec![json!({"content": "WROTE-OK"})]
    } else if said.contains("write") {
        vec![json!({"tool_calls": [call]})]
    } else if said.contains("evil") {
        vec![json!({"content": "EV\x1b[2J\rIL"})]
    } else if said.contains("hold") {
        std::thread::sleep(Duration::from_millis(1500));
        vec![json!({"content": "HELD-ANSWER"})]
    } else if said.contains("slow") {
        std::thread::sleep(Duration::from_secs(20));
        vec![json!({"content": "LATE"})]
    } else {
        vec![json!({"content": "STREAM-"}), json!({"content": "ANSWER"})]
    };
    let usage = json!({"prompt_tokens": 7, "completion_tokens": 3, "total_tokens": 10});
    if body["stream"].as_bool() == Some(true) {
        let mut out = String::new();
        for d in deltas {
            out.push_str(&format!(
                "data: {}\n\n",
                json!({"choices": [{"index": 0, "delta": d, "finish_reason": null}]})
            ));
        }
        out.push_str(&format!(
            "data: {}\n\n",
            json!({"choices": [], "usage": usage})
        ));
        out.push_str("data: [DONE]\n\n");
        reply("text/event-stream", out);
    } else {
        let d = &deltas[0];
        let msg = if d["tool_calls"].is_array() {
            json!({"role": "assistant", "content": null, "tool_calls": [{"id": "c1", "type": "function",
                "function": d["tool_calls"][0]["function"]}]})
        } else {
            json!({"role": "assistant", "content": "STREAM-ANSWER"})
        };
        reply(
            "application/json",
            json!({"id": "c", "object": "chat.completion", "created": 1, "model": "m",
                "choices": [{"index": 0, "message": msg, "finish_reason": "stop"}], "usage": usage})
            .to_string(),
        );
    }
}
