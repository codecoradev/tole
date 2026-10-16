//! #380 sibling change notices — end-to-end with the REAL tole binary and
//! TWO real child processes sharing one touch log.
//!
//! Child A reads `shared.txt`, then takes a slow model step (the mock holds
//! the answer); meanwhile child B overwrites `shared.txt`. A's watcher
//! folds the shared log, queues a notice, and the turn loop delivers it at
//! A's next safe point — so the model's third request carries a
//! "sibling notice" entry. The mock answers `A_NOTICED` only if it does.

#![cfg(feature = "shell-tools")]

use serde_json::{json, Value};
use std::io::{Read, Write};
use std::net::TcpListener;
use std::process::{Command, Stdio};
use std::time::Duration;

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

fn completion(msg: Value, finish: &str) -> Value {
    json!({
        "id": "chatcmpl-mock", "object": "chat.completion", "created": 1, "model": "mock",
        "choices": [{"index": 0, "message": msg, "finish_reason": finish}],
        "usage": {"prompt_tokens": 10, "completion_tokens": 5, "total_tokens": 15}
    })
}

fn tool_call(i: u64, name: &str, args: Value) -> Value {
    completion(
        json!({"role": "assistant", "content": null, "tool_calls": [{
            "id": format!("call_{i}"), "type": "function",
            "function": {"name": name, "arguments": args.to_string()}
        }]}),
        "tool_calls",
    )
}

fn final_text(t: &str) -> Value {
    completion(json!({"role": "assistant", "content": t}), "stop")
}

/// Content-driven: CHILD_A reads twice (the second model step is slow) and
/// reports whether a sibling notice reached it; CHILD_B overwrites the file.
fn decide(body: &Value) -> (Value, Duration) {
    let msgs = body["messages"].as_array().cloned().unwrap_or_default();
    let first_user = msgs
        .iter()
        .find(|m| m["role"] == "user")
        .and_then(|m| m["content"].as_str())
        .unwrap_or("")
        .to_string();
    let tool_results = msgs.iter().filter(|m| m["role"] == "tool").count();
    let notice_seen = msgs.iter().any(|m| {
        m["content"]
            .as_str()
            .is_some_and(|c| c.contains("sibling notice") && c.contains("child agent a-b"))
    });
    if first_user.contains("CHILD_A") {
        return match tool_results {
            0 => (
                tool_call(1, "read_file", json!({"path": "shared.txt"})),
                Duration::ZERO,
            ),
            // The slow step: B changes the file while this answer is held.
            1 => (
                tool_call(2, "read_file", json!({"path": "shared.txt", "limit": 5})),
                Duration::from_secs(4),
            ),
            _ if notice_seen => (final_text("A_NOTICED"), Duration::ZERO),
            _ => (final_text("A_BLIND"), Duration::ZERO),
        };
    }
    if tool_results == 0 {
        return (
            tool_call(
                1,
                "write_file",
                json!({"path": "shared.txt", "content": "changed by b"}),
            ),
            Duration::ZERO,
        );
    }
    (final_text("B_DONE"), Duration::ZERO)
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
                let streamed = body.get("stream").and_then(Value::as_bool) == Some(true);
                let (reply, delay) = decide(&body);
                std::thread::sleep(delay);
                let (ctype, payload) = if streamed {
                    ("text/event-stream", completion_to_sse(&reply))
                } else {
                    ("application/json", reply.to_string())
                };
                let resp = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: {ctype}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{payload}",
                    payload.len()
                );
                let _ = s.write_all(resp.as_bytes());
            });
        }
    });
    format!("http://{addr}/v1")
}

fn tole(
    base: &str,
    ws: &std::path::Path,
    log: &std::path::Path,
    id: &str,
    prompt: &str,
) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_tole"));
    cmd.args(["run", "--allow", "write_file", "--"])
        .arg(prompt)
        .current_dir(ws)
        .env("TOLE_BASE_URL", base)
        .env("TOLE_API_KEY", "test")
        .env("TOLE_MODEL", "mock")
        .env("TOLE_AGENT_ID", id)
        .env("TOLE_AGENT_TOUCH_LOG", log)
        .env_remove("TOLE_MEMORY")
        .env_remove("TOLE_TRUST")
        .env_remove("TOLE_SYSTEM_PROMPT")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    cmd
}

#[test]
fn a_sibling_edit_reaches_the_other_child_at_its_next_safe_point() {
    let base = spawn_mock();
    let root = std::env::temp_dir().join(format!("tole-sibling-e2e-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let ws = root.join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    std::fs::write(ws.join("shared.txt"), "original contents\n").unwrap();
    let log = root.join("tole-agents").join("touch.log");

    // Child A starts and reads the file, then waits on its slow model step.
    let a = tole(&base, &ws, &log, "a-a", "CHILD_A: read shared.txt twice")
        .spawn()
        .unwrap();
    std::thread::sleep(Duration::from_millis(1500));

    // Child B overwrites it while A is waiting.
    let b = tole(&base, &ws, &log, "a-b", "CHILD_B: overwrite shared.txt")
        .output()
        .unwrap();
    let b_out = String::from_utf8_lossy(&b.stdout);
    assert!(
        b.status.success(),
        "B failed: {b_out}\n{}",
        String::from_utf8_lossy(&b.stderr)
    );
    assert!(b_out.contains("B_DONE"), "{b_out}");
    assert_eq!(
        std::fs::read_to_string(ws.join("shared.txt")).unwrap(),
        "changed by b"
    );

    let a = a.wait_with_output().unwrap();
    let a_out = String::from_utf8_lossy(&a.stdout);
    let a_err = String::from_utf8_lossy(&a.stderr);
    assert!(a.status.success(), "A failed: {a_out}\n{a_err}");
    assert!(
        a_out.contains("A_NOTICED"),
        "A never got the notice: {a_out}\n{a_err}"
    );
    let _ = std::fs::remove_dir_all(&root);
}
