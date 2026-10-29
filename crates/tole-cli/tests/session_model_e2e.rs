//! #428: a session's durable `fact/model` register (set by ACP's model
//! picker, #176) is honored by every face — here `tole resume` and
//! `tole chat --resume`, which used to fall back to the env model.
//! End to end with the REAL binary and a mock provider that echoes the
//! model it was asked for.

use serde_json::{json, Value};
use std::io::{Read, Write};
use std::net::TcpListener;
use std::process::{Command, Stdio};
use std::time::Duration;
use tole_core::register::RegisterWrite;
use tole_core::storage::{Commit, JsonlStorage, Storage};

/// Mock: answers every request with a final `MODEL::<requested model>`.
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
                let content = format!("MODEL::{}", body["model"].as_str().unwrap_or("?"));
                let resp = if body["stream"].as_bool() == Some(true) {
                    let mut out = String::new();
                    for part in [content.as_str(), ""] {
                        out.push_str(&format!(
                            "data: {}\n\n",
                            json!({"choices": [{"index": 0,
                                "delta": {"content": part}, "finish_reason": null}]})
                        ));
                    }
                    out.push_str("data: [DONE]\n\n");
                    format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                        out.len(),
                        out
                    )
                } else {
                    let data = json!({
                        "id": "c", "object": "chat.completion", "created": 1, "model": "mock",
                        "choices": [{"index": 0, "message": {"role": "assistant",
                            "content": content}, "finish_reason": "stop"}]
                    })
                    .to_string();
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
    format!("http://{addr}/v1")
}

fn tole(url: &str, args: &[&str], stdin: &str) -> (String, String, i32) {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_tole"));
    cmd.args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .env("TOLE_BASE_URL", url)
        .env("TOLE_MODEL", "env-model")
        .env("TOLE_API_KEY", "k")
        .env("TOLE_NO_UPDATE_CHECK", "1")
        .env("PATH", "/usr/bin:/bin")
        .env_remove("TOLE_FALLBACK_BASE_URL")
        .env_remove("TOLE_MEMORY")
        .env_remove("TOLE_MEMORY_NAMESPACE")
        .env_remove("TOLE_SYSTEM_PROMPT")
        .env_remove("TOLE_TRUST")
        .env_remove("TOLE_AGENT_DEPTH");
    let mut child = cmd.spawn().expect("run tole");
    child
        .stdin
        .take()
        .unwrap()
        .write_all(stdin.as_bytes())
        .unwrap();
    let out = child.wait_with_output().unwrap();
    (
        String::from_utf8_lossy(&out.stdout).to_string(),
        String::from_utf8_lossy(&out.stderr).to_string(),
        out.status.code().unwrap_or(-1),
    )
}

/// A settled session whose model was picked in ACP.
fn picked_session(tag: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "tole-session-model-{tag}-{}-{:x}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let mut s = JsonlStorage::create(&dir, "s-picked", None).unwrap();
    s.commit(Commit::new().register(RegisterWrite::set("fact", "model", json!("picked-model"))))
        .unwrap();
    dir
}

#[test]
fn resume_uses_the_session_model() {
    let url = spawn_mock();
    let dir = picked_session("resume");
    let (out, err, code) = tole(
        &url,
        &["resume", "-s", dir.to_str().unwrap(), "s-picked", "go"],
        "",
    );
    assert_eq!(code, 0, "{err}");
    assert!(out.contains("MODEL::picked-model"), "{out}\n{err}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn chat_resume_uses_the_session_model() {
    let url = spawn_mock();
    let dir = picked_session("chat");
    let (out, err, code) = tole(
        &url,
        &["chat", "-s", dir.to_str().unwrap(), "--resume", "s-picked"],
        "go\n",
    );
    assert_eq!(code, 0, "{err}");
    assert!(out.contains("MODEL::picked-model"), "{out}\n{err}");
    let _ = std::fs::remove_dir_all(&dir);
}
