//! The mock provider: content-driven missions (the last user message decides).

use crate::sse::sse_response;
use serde_json::{json, Value};
use std::io::{Read, Write};
use std::net::TcpListener;
use std::time::Duration;

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
                // Only a tool result AFTER the last user message counts:
                // an earlier turn's result must not make this turn final.
                let has_tool_result = match msgs.iter().rposition(|m| m["role"] == "user") {
                    Some(i) => msgs[i + 1..].iter().any(|m| m["role"] == "tool"),
                    None => false,
                };
                let reply = if !has_tool_result {
                    let (path, content) = if last_user.contains("DELETE_MISSION") {
                        ("victim.txt", "bye")
                    } else if last_user.contains("SECOND_WRITE") {
                        ("mission-2.txt", "two")
                    } else if last_user.contains("PLAN_MISSION") {
                        ("plan.txt", "planned")
                    } else {
                        ("mission-1.txt", "one")
                    };
                    let (name, args) = if last_user.contains("DELETE_MISSION") {
                        ("delete_file", json!({"path": path}))
                    } else if last_user.contains("PLAN_MISSION") {
                        (
                            "update_plan",
                            json!({"entries": [
                                {"content": "read the spec", "status": "completed"},
                                {"content": "write the plan", "status": "in_progress"},
                            ]}),
                        )
                    } else {
                        ("write_file", json!({"path": path, "content": content}))
                    };
                    json!({
                        "id": "chatcmpl-mock", "object": "chat.completion", "created": 1,
                        "model": "mock",
                        "choices": [{"index": 0, "message": {"role": "assistant",
                            "content": null,
                            "reasoning": "I will act on the mission, then report.",
                            "tool_calls": [{"id": "call_1", "type": "function",
                                "function": {"name": name, "arguments": args.to_string()}}]},
                            "finish_reason": "tool_calls"}],
                        "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2}
                    })
                } else {
                    json!({
                        "id": "chatcmpl-mock", "object": "chat.completion", "created": 1,
                        "model": "mock",
                        "choices": [{"index": 0, "message": {"role": "assistant",
                            "content": "MISSION_DONE"}, "finish_reason": "stop"}],
                        "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2}
                    })
                };
                let resp = sse_response(&reply, streamed);
                let _ = s.write_all(resp.as_bytes());
            });
        }
    });
    format!("http://{addr}/v1")
}

pub fn provider_env() -> (String, Vec<(String, String)>) {
    let base = spawn_mock();
    (
        base.clone(),
        vec![
            ("TOLE_BASE_URL".to_string(), base),
            ("TOLE_MODEL".to_string(), "mock-model".to_string()),
            ("TOLE_API_KEY".to_string(), "sk-test".to_string()),
        ],
    )
}
