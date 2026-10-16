//! #383 child-agent handoff — end-to-end with the REAL tole binary.
//!
//! The test plays the parent model: it starts real child processes with
//! `AgentStartTool` and reads their state with `AgentPollTool`. The mock
//! provider drives the children:
//!
//! 1. an `implement` child submits a handoff whose verification FAILED →
//!    `agent_poll` reports `verify_failed` (not closed) with a fix prompt;
//! 2. a `fix` child started from that prompt reads the artifact BY
//!    REFERENCE with its own file tool (the parent never relays its
//!    content), submits passing verification → `closed`;
//! 3. an `implement` child that closes without any handoff is `unverified`.

#![cfg(feature = "shell-tools")]

use serde_json::{json, Value};
use std::io::{Read, Write};
use std::net::TcpListener;
use std::time::Duration;
use tole_core::agents::{AgentPollTool, AgentStartTool};
use tole_core::tool::Tool;

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

fn decide(body: &Value) -> Value {
    let msgs = body["messages"].as_array().cloned().unwrap_or_default();
    let first_user = msgs
        .iter()
        .find(|m| m["role"] == "user")
        .and_then(|m| m["content"].as_str())
        .unwrap_or("")
        .to_string();
    let results: Vec<String> = msgs
        .iter()
        .filter(|m| m["role"] == "tool")
        .map(|m| m["content"].as_str().unwrap_or("").to_string())
        .collect();
    if first_user.contains("IMPL_SILENT") {
        return final_text("IMPL_SILENT_DONE");
    }
    if first_user.contains("IMPL_TASK") {
        return match results.len() {
            0 => tool_call(
                1,
                "handoff_submit",
                json!({
                    "summary": "built it",
                    "artifacts": [{"path": "out.txt", "note": "the output"}],
                    "verify": {"command": "cargo test", "exit_code": 1, "summary": "1 failed"}
                }),
            ),
            _ => final_text("IMPL_DONE"),
        };
    }
    if first_user.contains("cannot be closed") {
        return match results.len() {
            // Read the artifact by reference with the child's own tool.
            0 => tool_call(1, "read_file", json!({"path": "out.txt"})),
            1 => {
                let saw = results[0].contains("ARTIFACT_BODY");
                tool_call(
                    2,
                    "handoff_submit",
                    json!({
                        "summary": if saw { "saw artifact" } else { "blind" },
                        "artifacts": [{"path": "out.txt"}],
                        "verify": {"command": "cargo test", "exit_code": 0, "summary": "ok"}
                    }),
                )
            }
            _ => final_text("FIX_DONE"),
        };
    }
    final_text("UNKNOWN_TASK")
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
                let reply = decide(&body);
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

/// A stand-in `tole` the children are started through: it pins the mock
/// provider and a no-op `uteke` (hermetic: the real memory store is never
/// touched), then execs the real binary.
#[cfg(unix)]
fn wrapper(root: &std::path::Path, base: &str) -> std::path::PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let bin_dir = root.join("bin");
    std::fs::create_dir_all(&bin_dir).unwrap();
    let uteke = bin_dir.join("uteke");
    std::fs::write(&uteke, "#!/bin/sh\necho '[]'\n").unwrap();
    let w = bin_dir.join("tole-wrapper");
    std::fs::write(
        &w,
        format!(
            "#!/bin/sh\nexport PATH=\"{}:$PATH\" TOLE_BASE_URL=\"{base}\" TOLE_API_KEY=test TOLE_MODEL=mock\nunset TOLE_TRUST TOLE_SYSTEM_PROMPT\nexec \"{}\" \"$@\"\n",
            bin_dir.display(),
            env!("CARGO_BIN_EXE_tole")
        ),
    )
    .unwrap();
    for f in [&uteke, &w] {
        std::fs::set_permissions(f, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    w
}

/// Poll until the child settles; returns the last poll result.
fn settle(poll: &AgentPollTool, id: &str) -> Value {
    let deadline = std::time::Instant::now() + Duration::from_secs(90);
    loop {
        let out = poll.execute(json!({"agent": id})).unwrap();
        if out["running"] == json!(false) {
            return out;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "child never settled: {out}"
        );
        std::thread::sleep(Duration::from_millis(300));
    }
}

#[cfg(unix)]
#[test]
fn a_failed_verification_is_not_closed_and_the_fix_child_reads_the_artifact_itself() {
    let base = spawn_mock();
    let root = std::env::temp_dir().join(format!("tole-handoff-e2e-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(root.join("out.txt"), "ARTIFACT_BODY\n").unwrap();
    let bin = wrapper(&root, &base);
    let start = AgentStartTool::new(&bin, root.clone());
    let poll = AgentPollTool::new(root.clone());

    // 1. implement child: verification failed.
    let s = start
        .execute(json!({"prompt": "IMPL_TASK build it", "role": "implement", "keep_mailbox": true}))
        .unwrap();
    let first = settle(&poll, s["agent"].as_str().unwrap());
    assert_eq!(first["role"], "implement", "{first}");
    assert_eq!(first["gate"], "verify_failed", "{first}");
    assert_eq!(first["handoff"]["artifacts"][0]["path"], "out.txt");
    let fix_prompt = first["fix_prompt"]
        .as_str()
        .expect("a fix prompt")
        .to_string();
    assert!(
        fix_prompt.contains("cargo test") && fix_prompt.contains("out.txt"),
        "{fix_prompt}"
    );

    // 2. fix child, started from that prompt: reads the artifact itself.
    let s = start
        .execute(json!({"prompt": fix_prompt, "role": "fix", "keep_mailbox": true}))
        .unwrap();
    let fix_id = s["agent"].as_str().unwrap().to_string();
    let second = settle(&poll, &fix_id);
    assert_eq!(second["gate"], "closed", "{second}");
    assert!(second.get("fix_prompt").is_none(), "{second}");
    assert_eq!(
        second["handoff"]["summary"], "saw artifact",
        "the fix child must have read out.txt itself: {second}"
    );

    // 3. an implement child that closes without a handoff is blocked.
    let s = start
        .execute(
            json!({"prompt": "IMPL_SILENT just finish", "role": "implement", "keep_mailbox": true}),
        )
        .unwrap();
    let third = settle(&poll, s["agent"].as_str().unwrap());
    assert_eq!(third["gate"], "unverified", "{third}");
    assert!(third["fix_prompt"]
        .as_str()
        .unwrap()
        .contains("without submitting a handoff"));
    let _ = std::fs::remove_dir_all(&root);
}
