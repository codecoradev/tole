//! #171 child agents — end-to-end with the REAL tole binary.
//!
//! A content-driven mock provider (no scenario files): the child's
//! first user message carries the CHILD_MISSION marker → immediate
//! final; the parent drives agent_start → agent_poll* → final, with
//! the agent id scraped from prior tool results. CI-safe: asserts on
//! the durable log tail (never the uteke mailbox — uteke need not be
//! installed; the child loop degrades gracefully without it).

use serde_json::{json, Value};
use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

// Minimal content-driven OpenAI-compatible server. Decision table:
// - first user message contains `CHILD_MISSION` → final "CHILD_ANSWER_42"
// - first user message contains `DEPTH_PROBE`   → agent_start call
//   (the depth-cap test: a child must NOT have that tool)
// - otherwise (parent): no tool results yet → agent_start; last tool
//   result `"running": true` → agent_poll; settled → final.

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

fn spawn_mock() -> (String, Arc<AtomicUsize>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let calls = Arc::new(AtomicUsize::new(0));
    let hits = Arc::clone(&calls);
    let _ = &hits; // reserved for request-count assertions
    let hits2 = Arc::clone(&calls);
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            // Thread per connection: the parent polls WHILE children
            // call concurrently — a single-threaded accept loop refuses
            // them (found live 2026-10-05).
            let hits3 = Arc::clone(&hits2);
            std::thread::spawn(move || {
                let mut s = stream;
                let mut buf = Vec::new();
                let mut chunk = [0u8; 16384];
                let mut body = Value::Null;
                // Read until the body after the header terminator parses
                // as JSON — no reliance on Content-Length casing/units
                // (the hand-rolled parser went blind on real requests).
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
                        break; // client closed; use whatever we have
                    }
                }
                let streamed = body.get("stream").and_then(Value::as_bool) == Some(true);
                hits3.fetch_add(1, Ordering::SeqCst);
                let reply = decide(&body);
                let resp = if streamed {
                    // The headers/body already arrived (body_in parsed it
                    // above); answer with the SSE form.
                    let body = completion_to_sse(&reply);
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
                };
                let _ = s.write_all(resp.as_bytes());
            });
        }
    });
    (format!("http://{addr}/v1"), calls)
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

fn decide(body: &Value) -> Value {
    let msgs = body["messages"].as_array().cloned().unwrap_or_default();
    let first_user = msgs
        .iter()
        .find(|m| m["role"] == "user")
        .and_then(|m| m["content"].as_str())
        .unwrap_or("")
        .to_string();
    if first_user.contains("CHILD_MISSION") {
        return completion(
            json!({"role": "assistant", "content": "CHILD_ANSWER_42"}),
            "stop",
        );
    }
    if first_user.contains("DEPTH_PROBE") {
        return tool_call(1, "agent_start", json!({"prompt": "grandchild?"}));
    }
    // parent: find the newest tool result
    let tool_results: Vec<&Value> = msgs
        .iter()
        .filter(|m| m["role"] == "tool" || m["role"] == "user")
        .filter(|m| {
            m["content"]
                .as_str()
                .map(|c| c.contains("\"agent\""))
                .unwrap_or(false)
        })
        .collect();
    let Some(last) = tool_results.last() else {
        return tool_call(
            1,
            "agent_start",
            json!({"prompt": "CHILD_MISSION: compute the answer to everything"}),
        );
    };
    let content = last["content"].as_str().unwrap_or("");
    let id = content
        .split("\"agent\":\"")
        .nth(1)
        .and_then(|rest| rest.split('"').next())
        .unwrap_or("a-unknown")
        .to_string();
    if content.contains("\"running\": true") || content.contains("\\\"running\\\": true") {
        return tool_call(2, "agent_poll", json!({"agent": id}));
    }
    completion(
        json!({"role": "assistant", "content": "PARENT_DONE"}),
        "stop",
    )
}

fn run_tole(env_extra: &[(&str, &str)], args: &[&str], stdin: &str) -> (String, String, i32) {
    let dir = std::env::temp_dir().join(format!(
        "tole-agent-e2e-{}-{:x}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let bin = env!("CARGO_BIN_EXE_tole");
    let mut cmd = std::process::Command::new(bin);
    cmd.args(args)
        .current_dir(&dir)
        .env("TOLE_API_KEY", "test")
        .env("TOLE_MODEL", "mock")
        .env_remove("TOLE_MEMORY")
        .env_remove("TOLE_TRUST")
        .env_remove("TOLE_SYSTEM_PROMPT")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    for (k, v) in env_extra {
        cmd.env(k, v);
    }
    let mut child = cmd.spawn().unwrap();
    child
        .stdin
        .as_mut()
        .unwrap()
        .write_all(stdin.as_bytes())
        .unwrap();
    let out = child.wait_with_output().unwrap();
    (
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
        out.status.code().unwrap_or(-1),
    )
}

#[test]
fn parent_spawns_child_polls_until_settled() {
    let (base, _hits) = spawn_mock();
    let (stdout, stderr, code) = run_tole(
        &[("TOLE_BASE_URL", base.as_str())],
        &[
            "run",
            "--allow",
            "agent_*",
            "spawn one child for the mission, poll until settled, then report",
        ],
        "",
    );
    assert_eq!(code, 0, "stdout={stdout}\nstderr={stderr}");
    assert!(
        stdout.contains("PARENT_DONE"),
        "stdout={stdout}\nstderr={stderr}"
    );
}

#[test]
fn depth_one_child_has_no_agent_tools() {
    let (base, _hits) = spawn_mock();
    let (stdout, stderr, code) = run_tole(
        &[("TOLE_BASE_URL", base.as_str()), ("TOLE_AGENT_DEPTH", "1")],
        &["run", "DEPTH_PROBE: try spawning a grandchild"],
        "",
    );
    // The child registry is built WITHOUT agent_start: the structural
    // depth cap — unknown tool, turn aborts durably.
    assert_ne!(code, 0);
    assert!(
        stderr.contains("unknown tool 'agent_start'"),
        "stderr={stderr}\nstdout={stdout}"
    );
}
