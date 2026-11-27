//! #1041 follow-up: the ACP `session/request_permission` title is the shared
//! plain-language summary, one safe line, bounded. The wire is the REAL
//! `tole acp` binary against a mock provider; the pure builder is also called
//! directly for the cases a registry request never produces (no summary).

use serde_json::{json, Value};
use tole_cli::acp_title::{permission_title, TITLE_CHARS};
use tole_core::action_summary::{ActionEffect, ActionKind, ActionSummary};
use tole_core::approval::ToolRequest;
use tole_core::tool::Risk;

fn request<'a>(
    input: &'a Value,
    description: &str,
    summary: Option<ActionSummary>,
) -> ToolRequest<'a> {
    ToolRequest {
        tool: "some_tool",
        risk: Risk::Write,
        input,
        description: description.to_string(),
        target: None,
        summary,
    }
}

fn is_one_safe_line(s: &str) -> bool {
    !s.chars()
        .any(|c| c.is_control() || matches!(c, '\u{2028}' | '\u{2029}' | '\u{202e}'))
}

#[test]
fn summary_is_rendered_instead_of_the_tool_text() {
    let input = json!({});
    let s = ActionSummary::new(
        ActionKind::DeleteFile,
        "src/a.rs",
        ActionEffect::CannotBeUndone,
        true,
    );
    let title = permission_title(&request(&input, "delete src/a.rs (tool text)", Some(s)));
    assert_eq!(title, "delete the file src/a.rs — Cannot be undone.");
}

#[test]
fn no_summary_falls_back_to_the_tool_text() {
    let input = json!({});
    assert_eq!(
        permission_title(&request(&input, "write 3 bytes to notes.txt", None)),
        "write 3 bytes to notes.txt"
    );
}

#[test]
fn a_tool_without_its_own_summary_keeps_its_text_after_the_summary() {
    let input = json!({"x": 1});
    let s = ActionSummary::unknown("git_push", &input);
    let title = permission_title(&request(&input, "push main to origin", Some(s)));
    assert!(
        title.starts_with("use the tool git_push — Effect unknown"),
        "{title}"
    );
    assert!(title.ends_with(":: push main to origin"), "{title}");
}

#[test]
fn control_and_bidi_characters_become_visible_escapes() {
    let input = json!({});
    let evil = "a\nb\r\u{1b}[31m\u{202e}c\u{2028}d";
    let s = ActionSummary::new(
        ActionKind::RunProgram,
        evil,
        ActionEffect::RunsProgram,
        false,
    );
    for title in [
        permission_title(&request(&input, evil, Some(s))),
        permission_title(&request(&input, evil, None)),
        permission_title(&request(
            &input,
            evil,
            Some(ActionSummary::unknown("t", &input)),
        )),
    ] {
        assert!(is_one_safe_line(&title), "{title:?}");
        assert!(
            title.contains("\\u{a}") && title.contains("\\u{1b}"),
            "{title}"
        );
    }
}

#[test]
fn a_cut_target_keeps_head_tail_and_the_effect_and_the_title_is_capped() {
    let input = json!({});
    let long = format!("HEAD{}TAIL", "x".repeat(5000));
    let s = ActionSummary::new(
        ActionKind::DeleteFile,
        &long,
        ActionEffect::CannotBeUndone,
        true,
    );
    let title = permission_title(&request(&input, "", Some(s)));
    assert!(
        title.chars().count() <= TITLE_CHARS,
        "{}",
        title.chars().count()
    );
    assert!(title.contains("HEAD") && title.contains("TAIL") && title.contains("more characters"));
    assert!(title.ends_with("Cannot be undone."), "{title}");
    let fallback = permission_title(&request(&input, &long, None));
    assert!(fallback.chars().count() <= TITLE_CHARS && fallback.ends_with("TAIL"));
    let huge = "y".repeat(100_000);
    assert!(
        permission_title(&request(&input, &huge, None))
            .chars()
            .count()
            <= TITLE_CHARS
    );
}

// ---------------------------------------------------------------------------
// The wire: the real `tole acp` binary (only built with `shell-tools`, #349)
// ---------------------------------------------------------------------------

#[cfg(feature = "shell-tools")]
mod wire {
    use crate::common::{self, TestDir};
    use serde_json::{json, Value};
    use std::io::{BufRead, Read, Write};
    use std::net::TcpListener;
    use std::process::Stdio;
    use std::time::{Duration, Instant};

    /// A mock provider: the last user message is `{"tool": .., "args": ..}`
    /// and the first reply calls exactly that tool; after a tool result it ends.
    fn spawn_mock() -> String {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                std::thread::spawn(move || serve_one(stream));
            }
        });
        format!("http://{addr}/v1")
    }

    fn serve_one(mut s: std::net::TcpStream) {
        let mut buf = Vec::new();
        let mut chunk = [0u8; 16384];
        let mut body = Value::Null;
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            let n = s.read(&mut chunk).unwrap_or(0);
            buf.extend_from_slice(&chunk[..n]);
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
        let streamed = body["stream"] == true;
        let msgs = body["messages"].as_array().cloned().unwrap_or_default();
        let last = msgs.iter().rposition(|m| m["role"] == "user").unwrap_or(0);
        let done = msgs.iter().skip(last + 1).any(|m| m["role"] == "tool");
        let call: Value = msgs
            .get(last)
            .and_then(|m| m["content"].as_str())
            .and_then(|t| serde_json::from_str(t).ok())
            .unwrap_or(Value::Null);
        let (content, calls) = if done {
            (json!("DONE"), Value::Null)
        } else {
            (
                Value::Null,
                json!([{"id": "call_1", "type": "function", "function": {
                    "name": call["tool"], "arguments": call["args"].to_string()}}]),
            )
        };
        let usage = json!({"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2});
        let (ctype, payload) = if streamed {
            let mut delta = json!({"content": content});
            if !calls.is_null() {
                let c = &calls[0];
                delta =
                    json!({"tool_calls": [{"index": 0, "id": c["id"], "function": c["function"]}]});
            }
            let finish = if done { "stop" } else { "tool_calls" };
            let chunk = |d: Value, f: Value| {
                format!(
                    "data: {}\n\n",
                    json!({"choices": [{"index": 0, "delta": d, "finish_reason": f}], "usage": null})
                )
            };
            let body = format!(
                "{}{}data: {}\n\ndata: [DONE]\n\n",
                chunk(delta, Value::Null),
                chunk(json!({}), json!(finish)),
                json!({"choices": [], "usage": usage})
            );
            ("text/event-stream", body)
        } else {
            let mut message = json!({"role": "assistant", "content": content});
            if !calls.is_null() {
                message["tool_calls"] = calls;
            }
            let finish = if done { "stop" } else { "tool_calls" };
            let reply = json!({"id": "c", "object": "chat.completion", "created": 1, "model": "mock",
                "choices": [{"index": 0, "message": message, "finish_reason": finish}], "usage": usage});
            ("application/json", reply.to_string())
        };
        let _ = write!(
            s,
            "HTTP/1.1 200 OK\r\nContent-Type: {ctype}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{payload}",
            payload.len()
        );
    }

    /// Run one prompt that makes the model call `tool` with `args`, answer
    /// the permission request with `reject-once`, and return the request's
    /// `params`.
    pub fn permission_params(
        tool: &str,
        args: Value,
        setup: impl FnOnce(&std::path::Path),
    ) -> Value {
        let base = spawn_mock();
        let home = TestDir::new("tole-acp-title-home");
        let cwd = TestDir::new("tole-acp-title-cwd");
        setup(cwd.path());
        let mut cmd = common::hermetic_command(env!("CARGO_BIN_EXE_tole"), home.path(), &[]);
        cmd.arg("acp")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .env("TOLE_BASE_URL", &base)
            .env("TOLE_MODEL", "mock-model")
            .env("TOLE_API_KEY", "sk-test")
            .env_remove("TOLE_MODELS");
        let mut child = cmd.spawn().expect("spawn tole acp");
        let mut stdin = child.stdin.take().unwrap();
        let stdout = child.stdout.take().unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            for line in std::io::BufReader::new(stdout)
                .lines()
                .map_while(Result::ok)
            {
                if let Ok(v) = serde_json::from_str::<Value>(&line) {
                    let _ = tx.send(v);
                }
            }
        });
        let mut send = |v: Value| {
            writeln!(stdin, "{v}").unwrap();
            stdin.flush().unwrap();
        };
        let mut params = Value::Null;
        let mut session = String::new();
        send(json!({"jsonrpc": "2.0", "id": 1, "method": "initialize",
            "params": {"protocolVersion": 1, "clientCapabilities": {}}}));
        let deadline = Instant::now() + Duration::from_secs(40);
        while Instant::now() < deadline {
            let Ok(msg) = rx.recv_timeout(Duration::from_secs(40)) else {
                break;
            };
            match (msg["method"].as_str(), msg["id"].as_u64()) {
                (Some("session/request_permission"), Some(id)) => {
                    params = msg["params"].clone();
                    send(json!({"jsonrpc": "2.0", "id": id,
                        "result": {"outcome": {"outcome": "selected", "optionId": "reject-once"}}}));
                }
                (None, Some(1)) => send(json!({"jsonrpc": "2.0", "id": 2, "method": "session/new",
                    "params": {"cwd": cwd.path().to_string_lossy()}})),
                (None, Some(2)) => {
                    session = msg["result"]["sessionId"].as_str().unwrap().to_string();
                    let text = json!({"tool": tool, "args": args}).to_string();
                    send(
                        json!({"jsonrpc": "2.0", "id": 3, "method": "session/prompt",
                        "params": {"sessionId": session, "prompt": [{"type": "text", "text": text}]}}),
                    );
                }
                (None, Some(3)) => break,
                _ => {}
            }
        }
        let _ = child.kill();
        let _ = child.wait();
        assert!(
            !session.is_empty() && !params.is_null(),
            "no permission request arrived"
        );
        params
    }
}

#[cfg(feature = "shell-tools")]
mod common;

#[cfg(feature = "shell-tools")]
#[test]
fn the_wire_title_is_the_summary_and_nothing_else_changes() {
    let p = wire::permission_params(
        "write_file",
        json!({"path": "notes.txt", "content": "hi"}),
        |_| {},
    );
    let call = &p["toolCall"];
    assert_eq!(
        call["title"],
        "create the file notes.txt — Adds a new file: it did not exist when asked. Nothing is replaced."
    );
    // Everything else on the request is what it was before the title changed.
    assert_eq!(call["toolCallId"], "perm-1");
    assert_eq!(call["name"], "write_file");
    assert_eq!(call["kind"], "edit");
    assert_eq!(
        call["rawInput"],
        json!({"path": "notes.txt", "content": "hi"})
    );
    let ids: Vec<&Value> = p["options"]
        .as_array()
        .unwrap()
        .iter()
        .map(|o| &o["optionId"])
        .collect();
    assert_eq!(ids, ["allow-once", "allow-always", "reject-once"]);
    let kinds: Vec<&Value> = p["options"]
        .as_array()
        .unwrap()
        .iter()
        .map(|o| &o["kind"])
        .collect();
    assert_eq!(kinds, ["allow_once", "allow_always", "reject_once"]);
}

#[cfg(feature = "shell-tools")]
#[test]
fn the_wire_title_of_a_destructive_call_keeps_its_two_options() {
    let p = wire::permission_params("delete_file", json!({"path": "victim.txt"}), |cwd| {
        std::fs::write(cwd.join("victim.txt"), "bye").unwrap();
    });
    assert_eq!(
        p["toolCall"]["title"],
        "delete the file victim.txt — Cannot be undone."
    );
    let ids: Vec<&Value> = p["options"]
        .as_array()
        .unwrap()
        .iter()
        .map(|o| &o["optionId"])
        .collect();
    assert_eq!(ids, ["allow-once", "reject-once"]);
}

#[cfg(feature = "shell-tools")]
#[test]
fn the_wire_title_neutralizes_a_model_sent_path_and_caps_a_long_one() {
    let evil = wire::permission_params(
        "write_file",
        json!({"path": "a\nb\u{1b}[31m.txt", "content": "x"}),
        |_| {},
    );
    let title = evil["toolCall"]["title"].as_str().unwrap();
    assert!(is_one_safe_line(title), "{title:?}");
    assert!(title.contains("a\\u{a}b\\u{1b}[31m.txt"), "{title}");

    let deep = (0..60)
        .map(|i| format!("dir{i:02}"))
        .collect::<Vec<_>>()
        .join("/");
    let long = wire::permission_params(
        "write_file",
        json!({"path": format!("{deep}/file.txt"), "content": "x"}),
        |_| {},
    );
    let title = long["toolCall"]["title"].as_str().unwrap();
    assert!(
        title.chars().count() <= TITLE_CHARS,
        "{}",
        title.chars().count()
    );
    assert!(
        title.contains("more characters") && title.ends_with("Nothing is replaced."),
        "{title}"
    );
    assert_eq!(long["toolCall"]["toolCallId"], "perm-1");
}
