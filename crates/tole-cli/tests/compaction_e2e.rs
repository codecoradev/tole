//! #440: context compaction end to end with the REAL binary — `run`, then
//! `resume` twice against a mock that reports large prompts. Before the
//! third turn the session compacts (window from `TOLE_CONTEXT_WINDOW` or
//! from the mock's `/models`), and the third turn's request carries the
//! fenced summary instead of the first turn.

use serde_json::{json, Value};
use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

static N: AtomicU64 = AtomicU64::new(0);

fn tmpdir(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!(
        "tole-compaction-e2e-{tag}-{}-{}",
        std::process::id(),
        N.fetch_add(1, Ordering::SeqCst)
    ));
    std::fs::create_dir_all(&d).unwrap();
    d.canonicalize().unwrap()
}

/// Mock provider. Chat completions answer `SUMMARY-TEXT` when asked to
/// summarize and `answer` otherwise, always reporting 9 000 prompt tokens.
/// `/models` lists model `m` with `models_window` (if any). Every chat
/// request body is recorded.
fn spawn_mock(models_window: Option<u64>) -> (String, Arc<Mutex<Vec<Value>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let seen = Arc::new(Mutex::new(Vec::new()));
    let log = seen.clone();
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let log = log.clone();
            std::thread::spawn(move || {
                let mut s = stream;
                let mut buf = Vec::new();
                let mut chunk = [0u8; 65536];
                let deadline = std::time::Instant::now() + Duration::from_secs(10);
                let mut body = Value::Null;
                let mut head = String::new();
                while std::time::Instant::now() < deadline {
                    let n = s.read(&mut chunk).unwrap_or(0);
                    buf.extend_from_slice(&chunk[..n]);
                    if let Some(h) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                        head = String::from_utf8_lossy(&buf[..h]).to_string();
                        if head.starts_with("GET") {
                            break;
                        }
                        if let Ok(v) = serde_json::from_slice::<Value>(&buf[h + 4..]) {
                            body = v;
                            break;
                        }
                    }
                    if n == 0 {
                        break;
                    }
                }
                let usage =
                    json!({"prompt_tokens": 9000, "completion_tokens": 5, "total_tokens": 9005});
                let (ctype, data) = if head.starts_with("GET") && head.contains("/models") {
                    let mut m = json!({"id": "m", "object": "model"});
                    if let Some(w) = models_window {
                        m["context_length"] = json!(w);
                    }
                    (
                        "application/json",
                        json!({"object": "list", "data": [m]}).to_string(),
                    )
                } else {
                    log.lock().unwrap().push(body.clone());
                    let summarize = body.to_string().contains("Summarize the conversation");
                    let text = if summarize { "SUMMARY-TEXT" } else { "answer" };
                    if body["stream"].as_bool() == Some(true) {
                        let mut out = String::new();
                        for part in [text, ""] {
                            out.push_str(&format!(
                                "data: {}\n\n",
                                json!({"choices": [{"index": 0,
                                    "delta": {"content": part}, "finish_reason": null}]})
                            ));
                        }
                        out.push_str(&format!(
                            "data: {}\n\n",
                            json!({"choices": [], "usage": usage})
                        ));
                        out.push_str("data: [DONE]\n\n");
                        ("text/event-stream", out)
                    } else {
                        let data = json!({
                            "id": "c", "object": "chat.completion", "created": 1, "model": "m",
                            "choices": [{"index": 0, "message": {"role": "assistant",
                                "content": text}, "finish_reason": "stop"}],
                            "usage": usage
                        });
                        ("application/json", data.to_string())
                    }
                };
                let resp = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: {ctype}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{data}",
                    data.len()
                );
                let _ = s.write_all(resp.as_bytes());
            });
        }
    });
    (format!("http://{addr}/v1"), seen)
}

fn tole(cwd: &Path, url: &str, args: &[&str], extra: &[(&str, &str)]) -> Output {
    let mut c = Command::new(env!("CARGO_BIN_EXE_tole"));
    c.args(args)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .env("CODECORA_HOME", cwd.join("home"))
        .env("TOLE_BASE_URL", url)
        .env("TOLE_MODEL", "m")
        .env("TOLE_API_KEY", "k")
        .env("TOLE_NO_UPDATE_CHECK", "1")
        .env("TOLE_NO_SESSION_INDEX", "1")
        .env("PATH", "/usr/bin:/bin")
        .env_remove("TOLE_CONTEXT_WINDOW")
        .env_remove("TOLE_FALLBACK_BASE_URL")
        .env_remove("TOLE_MEMORY")
        .env_remove("TOLE_MEMORY_NAMESPACE")
        .env_remove("TOLE_SYSTEM_PROMPT")
        .env_remove("TOLE_TRUST")
        .env_remove("TOLE_SKILL")
        .env_remove("TOLE_AGENT_DEPTH");
    for (k, v) in extra {
        c.env(k, v);
    }
    c.output().unwrap()
}

fn text(b: &[u8]) -> String {
    String::from_utf8_lossy(b).to_string()
}

/// run q1, resume q2, resume q3 — returns (stderr of the last resume,
/// session file text, the last chat request body).
fn three_turns(
    models_window: Option<u64>,
    extra: &[(&str, &str)],
    flags: &[&str],
) -> (String, String, Value) {
    let (url, seen) = spawn_mock(models_window);
    let d = tmpdir("s");
    let mut args = flags.to_vec();
    args.extend(["run", "first question about apples"]);
    let o = tole(&d, &url, &args, extra);
    assert!(o.status.success(), "{}", text(&o.stderr));
    let id = text(&o.stdout)
        .lines()
        .find_map(|l| l.strip_prefix("session: ").map(str::to_string))
        .unwrap();
    let mut last = String::new();
    for q in ["second question", "third question"] {
        let mut args = flags.to_vec();
        args.extend(["resume", id.as_str(), q]);
        let o = tole(&d, &url, &args, extra);
        assert!(o.status.success(), "{}", text(&o.stderr));
        last = text(&o.stderr);
    }
    let file = std::fs::read_to_string(d.join(format!(".tole/sessions/{id}.jsonl"))).unwrap();
    let body = seen.lock().unwrap().last().cloned().unwrap();
    (last, file, body)
}

fn assert_compacted(stderr: &str, file: &str, body: &Value) {
    assert!(stderr.contains("tole: context compacted"), "{stderr}");
    assert!(file.contains("\"compaction\""), "the entry is durable");
    let wire = body.to_string();
    assert!(wire.contains("SUMMARY-TEXT"), "{wire}");
    assert!(
        wire.contains("TOOL_RESULT_BEGIN"),
        "the summary is fenced: {wire}"
    );
    assert!(!wire.contains("apples"), "turn 1 left the wire: {wire}");
    assert!(wire.contains("second question") && wire.contains("third question"));
    // Append-only: turn 1 is still in the session file.
    assert!(file.contains("apples"));
}

#[test]
fn an_explicit_window_compacts_between_turns() {
    let (stderr, file, body) = three_turns(None, &[("TOLE_CONTEXT_WINDOW", "10000")], &[]);
    assert_compacted(&stderr, &file, &body);
}

#[test]
fn the_window_is_detected_from_models() {
    let (stderr, file, body) = three_turns(Some(10_000), &[], &[]);
    assert_compacted(&stderr, &file, &body);
}

#[test]
fn no_compaction_and_no_window_leave_the_wire_whole() {
    for (window, extra, flags) in [
        (Some(10_000), vec![], vec!["--no-compaction"]),
        (None, vec![], vec![]),
        (Some(10_000), vec![], vec!["--compact-at", "1.0"]),
    ] {
        let (stderr, file, body) = three_turns(window, &extra, &flags);
        assert!(!stderr.contains("context compacted"), "{stderr}");
        assert!(!file.contains("\"compaction\""));
        assert!(body.to_string().contains("apples"));
    }
    // An out-of-range fraction is refused up front.
    let (url, _) = spawn_mock(None);
    let o = tole(
        &tmpdir("bad"),
        &url,
        &["--compact-at", "1.5", "sessions"],
        &[],
    );
    assert!(!o.status.success());
}
