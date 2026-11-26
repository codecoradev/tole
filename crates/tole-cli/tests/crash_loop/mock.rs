//! A scripted provider for the crash loops: it answers from the CONTENT of the
//! request, so a resumed or restarted session meets the same script (a mock
//! that counted calls would not).
//!
//! - a request that mentions `HOLD-PROVIDER` is answered after `hold_ms`
//!   (a kill inside the model call);
//! - a turn whose last user message mentions `RUN-EFFECT` first asks for a `run_command`
//!   that appends a line to `effect.log` and then sleeps one second, so the
//!   test sees when the effect has started and can kill inside it;
//! - one that mentions `WRITE-FILE` asks for a `write_file` of `approved.txt`;
//! - anything else, and the turn after a tool result, is answered with
//!   [`ANSWER`], plus a `MISSION_COMPLETE` line when the conversation holds a
//!   mission goal or a mission continuation.

use serde_json::{json, Value};
use std::io::{Read, Write};
use std::net::TcpListener;
use std::time::{Duration, Instant};

/// The text of every plain answer.
pub const ANSWER: &str = "CRASH-LOOP-ANSWER";

/// The effect: one line appended to `effect.log` per run, then a second in
/// which the process can be killed. No shell is involved in `run_command`, so
/// `sh -c` is the argv.
pub const EFFECT: &str = "sh -c 'echo run >> effect.log; sleep 1'";

fn read_request(s: &mut std::net::TcpStream) -> Value {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 16384];
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        let n = s.read(&mut chunk).unwrap_or(0);
        buf.extend_from_slice(&chunk[..n]);
        if let Some(h) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            if let Ok(v) = serde_json::from_slice::<Value>(&buf[h + 4..]) {
                return v;
            }
        }
        if n == 0 {
            break;
        }
    }
    Value::Null
}

/// The tool call this conversation wants next, if any.
fn wanted_tool(body: &Value) -> Option<(&'static str, String)> {
    let msgs = body["messages"].as_array().cloned().unwrap_or_default();
    // The message that opened the turn: a denial note (`[tole] Your call to
    // ...`, a user-role message the host adds after a refused approval) does
    // not open one, so a turn that is replanned after an allow asks again.
    let last_user = msgs
        .iter()
        .rposition(|m| m["role"] == "user" && !m.to_string().contains("[tole] Your call to"))?;
    if msgs[last_user + 1..].iter().any(|m| m["role"] == "tool") {
        return None;
    }
    // Only the message that opens the turn: the history of a session that
    // ran the effect already must not ask for it again.
    let seen = msgs[last_user].to_string();
    if seen.contains("RUN-EFFECT") {
        Some(("run_command", json!({ "command": EFFECT }).to_string()))
    } else if seen.contains("WRITE-FILE") {
        Some((
            "write_file",
            json!({"path": "approved.txt", "content": "remote ok"}).to_string(),
        ))
    } else {
        None
    }
}

fn respond(body: &Value) -> String {
    let streamed = body.get("stream").and_then(Value::as_bool) == Some(true);
    let tool = wanted_tool(body);
    let mut text = ANSWER.to_string();
    let seen = body["messages"].to_string();
    if seen.contains("MISSION GOAL") || seen.contains("Mission continues") {
        text.push_str("\nMISSION_COMPLETE");
    }
    let usage = json!({"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2});
    if streamed {
        let delta = match &tool {
            Some((name, args)) => json!({"tool_calls": [{"index": 0, "id": "call_1",
                "function": {"name": name, "arguments": args}}]}),
            None => json!({"content": text}),
        };
        let out = format!(
            "data: {}\n\ndata: {}\n\ndata: [DONE]\n\n",
            json!({"choices": [{"index": 0, "delta": delta, "finish_reason": null}]}),
            json!({"choices": [], "usage": usage})
        );
        return format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{out}",
            out.len()
        );
    }
    let message = match &tool {
        Some((name, args)) => json!({"role": "assistant", "content": null, "tool_calls": [
            {"id": "call_1", "type": "function", "function": {"name": name, "arguments": args}}]}),
        None => json!({"role": "assistant", "content": text}),
    };
    let data = json!({"id": "c", "object": "chat.completion", "created": 1, "model": "mock",
        "choices": [{"index": 0, "message": message,
            "finish_reason": if tool.is_some() { "tool_calls" } else { "stop" }}],
        "usage": usage})
    .to_string();
    format!(
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{data}",
        data.len()
    )
}

/// Start the mock; the result is its base URL (`TOLE_BASE_URL`).
pub fn spawn(hold_ms: u64) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            std::thread::spawn(move || {
                let mut s = stream;
                let body = read_request(&mut s);
                if body.to_string().contains("HOLD-PROVIDER") {
                    std::thread::sleep(Duration::from_millis(hold_ms));
                }
                let _ = s.write_all(respond(&body).as_bytes());
            });
        }
    });
    format!("http://{addr}/v1")
}
