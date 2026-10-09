//! #199 `tole mission` — end-to-end with the REAL binary and a
//! content-driven mock: chaining, the completion marker, the verify
//! gate, and budget exhaustion that stays resumable.

use serde_json::{json, Value};
use std::io::{Read, Write};
use std::net::TcpListener;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

/// Scripted by TURN COUNT (per session): turn 1 works (todo_write) and
/// does NOT complete; turn 2 completes (with the marker) — unless
/// `require_file` is set, in which case completion comes only on the
/// turn after the file exists (exercising the verify gate).
fn spawn_mock(require_file: std::path::PathBuf) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let turns = Arc::new(AtomicUsize::new(0));
    let require_file = Arc::new(require_file);
    let t2 = Arc::clone(&turns);
    let rf0 = Arc::clone(&require_file);
    std::thread::spawn(move || {
        let require_file = rf0;
        for stream in listener.incoming().flatten() {
            let turns_conn = Arc::clone(&t2);
            let rf = Arc::clone(&require_file);
            std::thread::spawn(move || {
                let require_file = rf;
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
                let has_tool_result = match msgs.iter().rposition(|m| m["role"] == "user") {
                    Some(i) => msgs[i + 1..].iter().any(|m| m["role"] == "tool"),
                    None => false,
                };
                let n = turns_conn.fetch_add(1, Ordering::SeqCst);
                let file_there = require_file.exists();
                let reply = if has_tool_result {
                    let done = !require_file.is_file_path_dummy() && file_there
                        || require_file.is_file_path_dummy() && n >= 2;
                    let text = if done {
                        format!("The mission is done.\n{MISSION_COMPLETE}")
                    } else {
                        "Work continues.".to_string()
                    };
                    json!({
                        "id": "c", "object": "chat.completion", "created": 1, "model": "mock",
                        "choices": [{"index": 0, "message": {"role": "assistant",
                            "content": text}, "finish_reason": "stop"}],
                        "usage": {"prompt_tokens": 2, "completion_tokens": 2, "total_tokens": 4}
                    })
                } else if n == 0 {
                    json!({
                        "id": "c", "object": "chat.completion", "created": 1, "model": "mock",
                        "choices": [{"index": 0, "message": {"role": "assistant",
                            "content": null,
                            "tool_calls": [{"id": "call_1", "type": "function",
                                "function": {"name": "todo_write", "arguments":
                                    "{\"todos\":[{\"content\":\"do the work\",\"status\":\"in_progress\"}]}"}}]},
                            "finish_reason": "tool_calls"}],
                        "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2}
                    })
                } else {
                    // Continuation turn: create the required file (if the
                    // scenario wants one), then let the NEXT turn complete.
                    if !require_file.is_file_path_dummy() && !file_there {
                        json!({
                            "id": "c", "object": "chat.completion", "created": 1, "model": "mock",
                            "choices": [{"index": 0, "message": {"role": "assistant",
                                "content": null,
                                "tool_calls": [{"id": "call_2", "type": "function",
                                    "function": {"name": "write_file", "arguments":
                                        "{\"path\":\"done.marker\",\"content\":\"ok\"}"}}]},
                                "finish_reason": "tool_calls"}],
                            "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2}
                        })
                    } else {
                        json!({
                            "id": "c", "object": "chat.completion", "created": 1, "model": "mock",
                            "choices": [{"index": 0, "message": {"role": "assistant",
                                "content": format!("Halfway there.\n{MISSION_COMPLETE}")},
                                "finish_reason": "stop"}],
                            "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2}
                        })
                    }
                };
                let resp = if streamed {
                    let msg = &reply["choices"][0]["message"];
                    let mut out = String::new();
                    if let Some(c) = msg["content"].as_str() {
                        for part in [c, ""] {
                            out.push_str(&format!(
                                "data: {}\n\n",
                                json!({"choices": [{"index": 0,
                                    "delta": {"content": part}, "finish_reason": null}]})
                            ));
                        }
                    }
                    if let Some(tcs) = msg["tool_calls"].as_array() {
                        for (i, tc) in tcs.iter().enumerate() {
                            out.push_str(&format!(
                                "data: {}\n\n",
                                json!({"choices": [{"index": 0, "delta": {"tool_calls": [
                                    {"index": i, "id": tc["id"], "function": {
                                        "name": tc["function"]["name"],
                                        "arguments": tc["function"]["arguments"]}}]},
                                    "finish_reason": null}]})
                            ));
                        }
                    }
                    out.push_str(&format!(
                        "data: {}\n\n",
                        json!({"choices": [], "usage": {"prompt_tokens": 1,
                               "completion_tokens": 1, "total_tokens": 2,
                               "prompt_tokens_details": {"cached_tokens": 1}}})
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

// Helper for the mock's scenario switch (clippy-friendly, no magic bool).
trait FileProbe {
    fn is_file_path_dummy(&self) -> bool;
}
impl FileProbe for std::path::Path {
    fn is_file_path_dummy(&self) -> bool {
        self.as_os_str() == "NONE"
    }
}

const MISSION_COMPLETE: &str = "MISSION_COMPLETE";

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
        .env_remove("TOLE_AGENT_DEPTH")
        .env_remove("TOLE_STREAM");
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
        "tole-mission-{tag}-{}-{:x}",
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

/// The happy chain: turn 1 works, continuation completes, the durable
/// summary register lands, and `tole status` shows a resumable session.
#[test]
fn mission_chains_to_completion_with_summary() {
    let base = spawn_mock("NONE".into());
    let dir = temp_dir("chain");
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
            "mission",
            "-s",
            sessions.to_str().unwrap(),
            "--yes",
            "--max-steps",
            "12",
            "accomplish the thing",
        ],
    );
    assert_eq!(code, 0, "mission failed: {err}");
    assert!(out.contains("status: complete"), "{out}");
    assert!(out.contains("turns: 2"), "expected 2 chained turns: {out}");

    // Durable summary: the session's mission register is visible in the
    // replay — `tole status` still reports a clean session.
    let (status_out, status_err, status_code) = run_tole(
        &env_ref,
        &[
            "status",
            "-s",
            sessions.to_str().unwrap(),
            out.lines()
                .find_map(|l| l.strip_prefix("mission: "))
                .unwrap_or_default(),
        ],
    );
    assert_eq!(status_code, 0, "{status_err}");
    assert!(!status_out.is_empty());
    let _ = std::fs::remove_dir_all(&dir);
}

/// The verify gate: the model's completion marker alone is NOT enough —
/// the mission continues until `--verify` exits 0.
#[test]
fn verify_gate_overrides_the_completion_marker() {
    let dir = temp_dir("verify");
    let base = spawn_mock(dir.join("done.marker"));
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
            "mission",
            "-s",
            sessions.to_str().unwrap(),
            "--workspace",
            dir.to_str().unwrap(),
            "--yes",
            "--verify",
            &format!("test -f {}", dir.join("done.marker").display()),
            "make the marker",
        ],
    );
    assert_eq!(code, 0, "mission failed: {err}");
    assert!(out.contains("status: complete"), "{out}");
    assert!(
        dir.join("done.marker").exists(),
        "the work must have happened"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// Budget exhaustion: `--max-steps 1` settles after one turn with a
/// summary — and the session STAYS resumable (never a dead session).
#[test]
fn budget_exhaustion_settles_resumably() {
    let base = spawn_mock("NONE".into());
    let dir = temp_dir("budget");
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
            "mission",
            "-s",
            sessions.to_str().unwrap(),
            "--yes",
            "--max-steps",
            "1",
            "impossible in one turn",
        ],
    );
    assert_ne!(
        code, 0,
        "budget exhaustion must exit nonzero (issue #253): {err}"
    );
    assert!(out.contains("status: exhausted_steps"), "{out}");
    // The session is a normal durable session: `resume` with a prompt
    // runs another turn and completes the mission.
    let session_id = out
        .lines()
        .find_map(|l| l.strip_prefix("mission: "))
        .expect("session id")
        .to_string();
    let (out2, err2, code2) = run_tole(
        &env_ref,
        &[
            "resume",
            "-s",
            sessions.to_str().unwrap(),
            &session_id,
            "continue the mission",
        ],
    );
    assert_eq!(code2, 0, "resume must work after exhaustion: {err2}");
    assert!(
        out2.contains("Halfway there") || out2.contains("done"),
        "the chained turn ran: {out2}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// Issue #201: a token ceiling trips `exhausted_tokens`, and the
/// durable cost report (with tier + tool-call counts) is visible via
/// `tole status`.
#[test]
fn token_budget_and_cost_report_visible_in_status() {
    let base = spawn_mock("NONE".into());
    let dir = temp_dir("tokens");
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
            "mission",
            "-s",
            sessions.to_str().unwrap(),
            "--yes",
            "--max-tokens",
            "1",
            "work until tokens run out",
        ],
    );
    assert_ne!(
        code, 0,
        "exhausted_tokens must exit nonzero (issue #253): {err}"
    );
    assert!(out.contains("status: exhausted_tokens"), "{out}");

    let session_id = out
        .lines()
        .find_map(|l| l.strip_prefix("mission: "))
        .expect("session id")
        .to_string();
    let (status_out, status_err, status_code) = run_tole(
        &env_ref,
        &["status", "-s", sessions.to_str().unwrap(), &session_id],
    );
    assert_eq!(status_code, 0, "{status_err}");
    assert!(
        status_out.contains("mission:"),
        "status must render the cost report: {status_out}"
    );
    assert!(
        status_out.contains("exhausted_tokens") && status_out.contains("tool_calls"),
        "the report carries status + per-tier counts: {status_out}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// #211: the settle-time `fact/mission` register gains an additive
/// `cached_tokens`, and the budget-facing fields are unchanged: every
/// mock step reports prompt 1 + completion 1 (cached 1), so
/// `tokens_in_out == 2 * steps` and `cached_tokens == steps`.
#[test]
fn mission_register_reports_cached_tokens_without_touching_budget_fields() {
    let base = spawn_mock("NONE".into());
    let dir = temp_dir("cached");
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
            "mission",
            "-s",
            sessions.to_str().unwrap(),
            "--yes",
            "--max-steps",
            "12",
            "accomplish the thing",
        ],
    );
    assert_eq!(code, 0, "mission failed: {err}");
    let session_id = out
        .lines()
        .find_map(|l| l.strip_prefix("mission: "))
        .expect("session id")
        .to_string();
    let (status_out, status_err, status_code) = run_tole(
        &env_ref,
        &["status", "-s", sessions.to_str().unwrap(), &session_id],
    );
    assert_eq!(status_code, 0, "{status_err}");
    let register: Value = serde_json::from_str(
        status_out
            .lines()
            .find_map(|l| l.strip_prefix("mission: "))
            .expect("mission register line"),
    )
    .expect("register is JSON");
    let steps = register["steps"].as_u64().unwrap();
    assert!(steps >= 2, "{register}");
    assert_eq!(register["tokens_in_out"], json!(2 * steps), "{register}");
    assert_eq!(register["cached_tokens"], json!(steps), "{register}");
    // The real binary also stores the wire split: the status block
    // renders it (first/last step, not n/a).
    assert!(
        status_out.contains(&format!("steps:   {steps}")),
        "{status_out}"
    );
    assert!(status_out.contains("first step:"), "{status_out}");
    assert!(status_out.contains("(hit rate 100.0%)"), "{status_out}");
    let _ = std::fs::remove_dir_all(&dir);
}
