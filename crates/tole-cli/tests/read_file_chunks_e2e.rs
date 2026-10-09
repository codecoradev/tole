//! #211 part 3 — the model-facing `read_file` continuation contract,
//! end to end with the REAL binary and a scripted mock provider: a
//! 50,000-char file is read in 3 chunks by following `next_offset`, and
//! the mission concludes. No network.

use serde_json::{json, Value};
use std::io::{Read, Write};
use std::net::TcpListener;
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};

/// Scripted mock: while the latest tool result carries `"next_offset":N`
/// (or there is no tool result yet) it asks for `read_file` at that
/// offset; once the result is not truncated it gives the final answer.
/// Every requested offset is recorded in `offsets`.
fn spawn_mock(offsets: Arc<Mutex<Vec<u64>>>) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let offsets = offsets.clone();
            std::thread::spawn(move || {
                let mut s = stream;
                let mut buf = Vec::new();
                let mut chunk = [0u8; 16384];
                let mut body = Value::Null;
                let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
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
                let streamed = body.get("stream").and_then(Value::as_bool) == Some(true);
                let msgs = body["messages"].as_array().cloned().unwrap_or_default();
                let last_tool = msgs
                    .iter()
                    .rev()
                    .take_while(|m| m["role"] != "user")
                    .find(|m| m["role"] == "tool")
                    .and_then(|m| m["content"].as_str())
                    .map(str::to_string);
                // Next offset to request, or None when the read is done.
                let next: Option<u64> = match &last_tool {
                    None => Some(0),
                    Some(t) => t.split("\"next_offset\":").nth(1).map(|r| {
                        r.trim_start()
                            .chars()
                            .take_while(char::is_ascii_digit)
                            .collect::<String>()
                            .parse()
                            .expect("next_offset digits")
                    }),
                };
                let msg = match next {
                    Some(off) => {
                        offsets.lock().unwrap().push(off);
                        let args = if off == 0 {
                            json!({ "path": "big.txt" })
                        } else {
                            json!({ "path": "big.txt", "offset": off })
                        };
                        json!({"role": "assistant", "content": null,
                            "tool_calls": [{"id": format!("call_{off}"), "type": "function",
                                "function": {"name": "read_file", "arguments": args.to_string()}}]})
                    }
                    None => {
                        let done = last_tool
                            .unwrap_or_default()
                            .contains("\"truncated\":false");
                        json!({"role": "assistant", "content": format!("CHUNKS_DONE untruncated={done}")})
                    }
                };
                let finish = if next.is_some() { "tool_calls" } else { "stop" };
                let usage = json!({"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2});
                let resp = if streamed {
                    let mut out = String::new();
                    let delta = if let Some(tcs) = msg["tool_calls"].as_array() {
                        json!({"tool_calls": [{"index": 0, "id": tcs[0]["id"],
                            "function": {"name": tcs[0]["function"]["name"],
                                         "arguments": tcs[0]["function"]["arguments"]}}]})
                    } else {
                        json!({"content": msg["content"]})
                    };
                    out.push_str(&format!(
                        "data: {}\n\n",
                        json!({"choices": [{"index": 0, "delta": delta, "finish_reason": null}]})
                    ));
                    out.push_str(&format!(
                        "data: {}\n\n",
                        json!({"choices": [], "usage": usage})
                    ));
                    out.push_str("data: [DONE]\n\n");
                    format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                        out.len(), out
                    )
                } else {
                    let data = json!({
                        "id": "c", "object": "chat.completion", "created": 1, "model": "mock",
                        "choices": [{"index": 0, "message": msg, "finish_reason": finish}],
                        "usage": usage
                    })
                    .to_string();
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

#[test]
fn mission_reads_a_50k_char_file_in_three_chunks() {
    let offsets = Arc::new(Mutex::new(Vec::new()));
    let base = spawn_mock(offsets.clone());
    let dir = std::env::temp_dir().join(format!(
        "tole-readfile-e2e-{}-{:x}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let ws = dir.join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    // 50,000 chars including multibyte ones (so bytes != chars).
    let text: String = "abcdé日😀".chars().cycle().take(50_000).collect();
    assert_eq!(text.chars().count(), 50_000);
    std::fs::write(ws.join("big.txt"), &text).unwrap();

    let out = Command::new(env!("CARGO_BIN_EXE_tole"))
        .args([
            "--workspace",
            ws.to_str().unwrap(),
            "run",
            "-s",
            dir.join("sessions").to_str().unwrap(),
            "--yes",
            "--",
            "read big.txt completely",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .env_remove("TOLE_MEMORY")
        .env_remove("TOLE_MEMORY_NAMESPACE")
        .env_remove("TOLE_SYSTEM_PROMPT")
        .env_remove("TOLE_TRUST")
        .env_remove("TOLE_AGENT_DEPTH")
        .env("TOLE_BASE_URL", &base)
        .env("TOLE_MODEL", "mock-model")
        .env("TOLE_API_KEY", "sk-test")
        .output()
        .expect("run tole");
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(0), "run failed: {stderr}");
    assert!(
        stdout.contains("CHUNKS_DONE untruncated=true"),
        "mission must conclude after the last chunk: {stdout}"
    );
    assert_eq!(
        *offsets.lock().unwrap(),
        vec![0, 20_000, 40_000],
        "the model follows next_offset: 3 chunks, 20k each"
    );
    let _ = std::fs::remove_dir_all(&dir);
}
