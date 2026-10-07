//! #216 run ergonomics — end-to-end with the REAL binary and the mock
//! provider: --prompt-file, --name (sessions listing + resume by name),
//! and the wall-clock timeout settling resumably.

use serde_json::{json, Value};
use std::io::{Read, Write};
use std::net::TcpListener;
use std::process::{Command, Stdio};
use std::time::Duration;

/// Mock: `SLOW` prompts sleep 5 s before the final (exercises the
/// timeout); every other prompt answers a final echoing the prompt.
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
                let last_user = body["messages"]
                    .as_array()
                    .and_then(|m| m.last())
                    .and_then(|m| m["content"].as_str())
                    .unwrap_or("")
                    .to_string();
                if last_user.contains("SLOW") {
                    std::thread::sleep(Duration::from_secs(5));
                }
                let reply = json!({
                    "id": "c", "object": "chat.completion", "created": 1, "model": "mock",
                    "choices": [{"index": 0, "message": {"role": "assistant",
                        "content": format!("ECHO::{last_user}")}, "finish_reason": "stop"}],
                    "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2}
                });
                let resp = if streamed {
                    let msg = &reply["choices"][0]["message"];
                    let content = msg["content"].as_str().unwrap_or("");
                    let mut out = String::new();
                    for part in [content, ""] {
                        out.push_str(&format!(
                            "data: {}\n\n",
                            json!({"choices": [{"index": 0,
                                "delta": {"content": part}, "finish_reason": null}]})
                        ));
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

fn temp_dir(tag: &str) -> std::path::PathBuf {
    let d = std::env::temp_dir().join(format!(
        "tole-ergo-{tag}-{}-{:x}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

/// --prompt-file feeds the prompt (the echo proves it reached the
/// model), --name pins the header alias, `tole sessions` shows it, and
/// `resume` accepts the NAME instead of the generated id.
#[test]
fn prompt_file_name_and_resume_by_name() {
    let base = spawn_mock();
    let dir = temp_dir("name");
    let env = [
        ("TOLE_BASE_URL".to_string(), base),
        ("TOLE_MODEL".to_string(), "mock-model".to_string()),
        ("TOLE_API_KEY".to_string(), "sk-test".to_string()),
    ];
    let env_ref: Vec<(&str, &str)> = env.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
    let sessions = dir.join("sessions");
    let prompt_file = dir.join("instructions.txt");
    std::fs::write(&prompt_file, "PROMPT_FROM_FILE please").unwrap();

    let (out, err, code) = run_tole(
        &env_ref,
        &[
            "run",
            "-s",
            sessions.to_str().unwrap(),
            "--name",
            "batch-1",
            "--prompt-file",
            prompt_file.to_str().unwrap(),
        ],
    );
    assert_eq!(code, 0, "{err}");
    assert!(
        out.contains("ECHO::PROMPT_FROM_FILE"),
        "file content reached the model: {out}"
    );
    assert!(out.contains("name: batch-1"), "{out}");

    // sessions lists the alias.
    let (list, _err, code) = run_tole(&env_ref, &["sessions", "-s", sessions.to_str().unwrap()]);
    assert_eq!(code, 0);
    assert!(
        list.contains("batch-1"),
        "the alias appears in sessions: {list}"
    );

    // resume accepts the NAME (a valid-session-id-shaped string would
    // have matched directly; batch-1 contains a dash but no file → the
    // name scan must resolve it).
    let (out2, err2, code2) = run_tole(
        &env_ref,
        &[
            "resume",
            "-s",
            sessions.to_str().unwrap(),
            "batch-1",
            "again",
        ],
    );
    assert_eq!(code2, 0, "{err2}");
    assert!(out2.contains("ECHO::"), "{out2}");
    let _ = std::fs::remove_dir_all(&dir);
}

/// --prompt-file and positional PROMPT are mutually exclusive; neither
/// is a loud error.
#[test]
fn prompt_source_conflicts_are_loud() {
    let base = spawn_mock();
    let dir = temp_dir("conflict");
    let env = [
        ("TOLE_BASE_URL".to_string(), base),
        ("TOLE_MODEL".to_string(), "mock-model".to_string()),
        ("TOLE_API_KEY".to_string(), "sk-test".to_string()),
    ];
    let env_ref: Vec<(&str, &str)> = env.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
    let sessions = dir.join("sessions");
    let pf = dir.join("p.txt");
    std::fs::write(&pf, "x").unwrap();
    let (_, err, code) = run_tole(
        &env_ref,
        &[
            "run",
            "-s",
            sessions.to_str().unwrap(),
            "--prompt-file",
            pf.to_str().unwrap(),
            "positional",
        ],
    );
    assert_ne!(code, 0);
    assert!(err.contains("not both"), "{err}");
    let (_, err2, code2) = run_tole(&env_ref, &["run", "-s", sessions.to_str().unwrap()]);
    assert_ne!(code2, 0);
    assert!(err2.contains("missing prompt"), "{err2}");
    let _ = std::fs::remove_dir_all(&dir);
}

/// --timeout 2 with a 5 s mock: the turn cancels at a checkpoint,
/// settles resumably (exit code 8 per the #178 convention), and a
/// follow-up resume completes normally.
#[test]
fn wall_clock_timeout_settles_resumably() {
    let base = spawn_mock();
    let dir = temp_dir("timeout");
    let env = [
        ("TOLE_BASE_URL".to_string(), base),
        ("TOLE_MODEL".to_string(), "mock-model".to_string()),
        ("TOLE_API_KEY".to_string(), "sk-test".to_string()),
    ];
    let env_ref: Vec<(&str, &str)> = env.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
    let sessions = dir.join("sessions");
    let (out, err, code) = run_tole(
        &env_ref,
        &[
            "run",
            "-s",
            sessions.to_str().unwrap(),
            "--timeout",
            "2",
            "SLOW mission",
        ],
    );
    assert_eq!(code, 8, "the #178 cancelled-exit convention: {err}");
    assert!(
        out.contains("cancelled") || err.contains("cancelled"),
        "{out} {err}"
    );

    // The session is resumable: a normal follow-up completes.
    let session_id = out
        .lines()
        .find_map(|l| l.strip_prefix("session: "))
        .expect("session id")
        .to_string();
    let (out2, err2, code2) = run_tole(
        &env_ref,
        &[
            "resume",
            "-s",
            sessions.to_str().unwrap(),
            &session_id,
            "now answer quickly",
        ],
    );
    assert_eq!(code2, 0, "{err2}");
    assert!(out2.contains("ECHO::"), "{out2}");
    let _ = std::fs::remove_dir_all(&dir);
}
