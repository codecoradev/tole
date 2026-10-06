//! #198 todo tools — end-to-end with the REAL binary: a first `tole
//! run` writes a task list, a SECOND process (`tole resume`) reads it
//! back — hydration from the replayed transcript is the whole point.

use serde_json::{json, Value};
use std::io::{Read, Write};
use std::net::TcpListener;
use std::process::{Command, Stdio};

/// Content-driven mock: TODO_MISSION → todo_write; READ_MISSION →
/// todo_read; the step after a tool result → final quoting the result.
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
                let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
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
                let has_tool_result = match msgs.iter().rposition(|m| m["role"] == "user") {
                    Some(i) => msgs[i + 1..].iter().any(|m| m["role"] == "tool"),
                    None => false,
                };
                let reply = if has_tool_result {
                    // Echo the settled todo state into the final answer.
                    let last_tool = msgs
                        .iter()
                        .rev()
                        .find(|m| m["role"] == "tool")
                        .and_then(|m| m["content"].as_str())
                        .unwrap_or("")
                        .to_string();
                    json!({
                        "id": "c", "object": "chat.completion", "created": 1, "model": "mock",
                        "choices": [{"index": 0, "message": {"role": "assistant",
                            "content": format!("TODO_STATE::{last_tool}")},
                            "finish_reason": "stop"}],
                        "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2}
                    })
                } else if last_user.contains("TODO_MISSION") {
                    json!({
                        "id": "c", "object": "chat.completion", "created": 1, "model": "mock",
                        "choices": [{"index": 0, "message": {"role": "assistant",
                            "content": null,
                            "tool_calls": [{"id": "call_1", "type": "function",
                                "function": {"name": "todo_write", "arguments":
                                    "{\"todos\":[{\"id\":\"t1\",\"content\":\"plan the mission\",\"status\":\"completed\"},{\"id\":\"t2\",\"content\":\"execute it\",\"status\":\"in_progress\"}]}"}}]},
                            "finish_reason": "tool_calls"}],
                        "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2}
                    })
                } else {
                    json!({
                        "id": "c", "object": "chat.completion", "created": 1, "model": "mock",
                        "choices": [{"index": 0, "message": {"role": "assistant",
                            "content": null,
                            "tool_calls": [{"id": "call_2", "type": "function",
                                "function": {"name": "todo_read", "arguments": "{}"}}]},
                            "finish_reason": "tool_calls"}],
                        "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2}
                    })
                };
                let resp = if streamed {
                    // Minimal SSE form of the same reply (phase-3 mocks).
                    let msg = &reply["choices"][0]["message"];
                    let mut out = String::new();
                    if let Some(c) = msg["content"].as_str() {
                        for part in [c, ""] {
                            let d = json!({"content": part});
                            out.push_str(&format!(
                                "data: {}\n\n",
                                json!({"choices": [{"index": 0, "delta": d, "finish_reason": null}]})
                            ));
                        }
                    }
                    if let Some(tcs) = msg["tool_calls"].as_array() {
                        for (i, tc) in tcs.iter().enumerate() {
                            let d = json!({"tool_calls": [{"index": i, "id": tc["id"],
                                "function": {
                                    "name": tc["function"]["name"],
                                    "arguments": tc["function"]["arguments"]}}]});
                            out.push_str(&format!(
                                "data: {}\n\n",
                                json!({"choices": [{"index": 0, "delta": d, "finish_reason": null}]})
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

fn run_tole(env: &[(&str, &str)], args: &[&str]) -> (String, String, i32) {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_tole"));
    cmd.args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .env_remove("TOLE_MEMORY")
        .env_remove("TOLE_MEMORY_NAMESPACE")
        .env_remove("TOLE_SYSTEM_PROMPT")
        .env_remove("TOLE_TRUST")
        .env_remove("TOLE_AGENT_DEPTH");
    for (k, v) in env {
        cmd.env(k, v);
    }
    let out = cmd.output().expect("run tole");
    (
        String::from_utf8_lossy(&out.stdout).to_string(),
        String::from_utf8_lossy(&out.stderr).to_string(),
        out.status.code().unwrap_or(-1),
    )
}

#[test]
fn todo_list_survives_process_boundary_via_resume() {
    let base = spawn_mock();
    let dir = std::env::temp_dir().join(format!(
        "tole-todo-e2e-{}-{:x}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let env = [
        ("TOLE_BASE_URL".to_string(), base.clone()),
        ("TOLE_MODEL".to_string(), "mock-model".to_string()),
        ("TOLE_API_KEY".to_string(), "sk-test".to_string()),
    ];
    let env_ref: Vec<(&str, &str)> = env.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
    let sessions = dir.join("sessions");
    let sessions_arg = format!("{}", sessions.display());

    // Turn 1: write the list (auto-approved via --trust internal, which
    // covers todo_write — the documented preset contract).
    let (out1, err1, code1) = run_tole(
        &env_ref,
        &[
            "run",
            "-s",
            &sessions_arg,
            "--yes",
            "--",
            "TODO_MISSION plan it",
        ],
    );
    assert_eq!(code1, 0, "run failed: {err1}");
    let session_id = out1
        .lines()
        .find_map(|l| l.strip_prefix("session: "))
        .expect("session id on stdout")
        .to_string();

    // Turn 2 in a FRESH process: todo_read must return the hydrated list.
    let (out2, err2, code2) = run_tole(
        &env_ref,
        &[
            "resume",
            "-s",
            &sessions_arg,
            &session_id,
            "READ_MISSION read it back",
        ],
    );
    assert_eq!(code2, 0, "resume failed: {err2}");
    assert!(
        out2.contains("TODO_STATE::"),
        "final answer must echo the todo state: {out2}"
    );
    assert!(
        out2.contains("plan the mission") && out2.contains("execute it"),
        "the hydrated list from process 1 must come back: {out2}"
    );
    assert!(
        out2.contains("in_progress"),
        "statuses survive the fold: {out2}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}
