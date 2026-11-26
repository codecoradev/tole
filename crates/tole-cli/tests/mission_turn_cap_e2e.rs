//! #1239: a turn that ends at the per-turn model-step cap (`MAX_STEPS`, 32) is
//! not the end of a `tole mission`. The REAL binary against a counting mock:
//! the mission continues while its own budgets allow, stops after
//! `TURN_CAP_TRIPS` such turns in a row with a status that names the per-turn
//! cap, and a mission budget still ends it first.

mod common;
use common::TestDir;
use serde_json::{json, Value};
use std::io::{Read, Write};
use std::net::TcpListener;
use std::process::Stdio;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

/// The per-turn cap of `tole-core` and the mission's patience for it.
const TURN_CAP: usize = tole_core::turn::MAX_STEPS;
const TRIPS: u64 = 3;

/// A mock that answers every request with a `todo_write` call whose input
/// differs each time (so the repeated-call loop guard never fires) until
/// `finish_at` requests were served; from then on it declares the mission
/// complete. `usize::MAX` never finishes.
fn spawn_mock(finish_at: usize) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let served = Arc::new(AtomicUsize::new(0));
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let served = Arc::clone(&served);
            std::thread::spawn(move || {
                let mut s = stream;
                let (mut buf, mut chunk) = (Vec::new(), [0u8; 16384]);
                let mut streamed = false;
                let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
                while std::time::Instant::now() < deadline {
                    let n = s.read(&mut chunk).unwrap_or(0);
                    buf.extend_from_slice(&chunk[..n]);
                    if let Some(h) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                        if let Ok(v) = serde_json::from_slice::<Value>(&buf[h + 4..]) {
                            streamed = v["stream"].as_bool() == Some(true);
                            break;
                        }
                    }
                    if n == 0 {
                        break;
                    }
                }
                let n = served.fetch_add(1, Ordering::SeqCst);
                let usage = json!({"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2});
                let (content, calls) = if n >= finish_at {
                    (json!("All done.\nMISSION_COMPLETE"), json!([]))
                } else {
                    let args = json!({"todos": [{"content": format!("step {n}"),
                                                 "status": "in_progress"}]});
                    let call = json!({"index": 0, "id": format!("call_{n}"), "type": "function",
                        "function": {"name": "todo_write", "arguments": args.to_string()}});
                    (Value::Null, json!([call]))
                };
                let (ctype, body) = if streamed {
                    let mut delta = json!({"content": content});
                    if !calls.as_array().unwrap().is_empty() {
                        delta = json!({"tool_calls": calls});
                    }
                    let chunk = json!({"choices": [{"index": 0, "delta": delta,
                                                    "finish_reason": null}]});
                    let tail = json!({"choices": [], "usage": usage});
                    let sse = format!("data: {chunk}\n\ndata: {tail}\n\ndata: [DONE]\n\n");
                    ("text/event-stream", sse)
                } else {
                    let reply = json!({"id": "c", "object": "chat.completion", "created": 1,
                        "model": "mock", "usage": usage,
                        "choices": [{"index": 0, "finish_reason": "stop", "message":
                            {"role": "assistant", "content": content, "tool_calls": calls}}]});
                    ("application/json", reply.to_string())
                };
                let _ = write!(
                    s,
                    "HTTP/1.1 200 OK\r\nContent-Type: {ctype}\r\nContent-Length: {}\r\n\
                     Connection: close\r\n\r\n{body}",
                    body.len()
                );
            });
        }
    });
    format!("http://{addr}/v1")
}

/// What `tole mission` printed and how it exited.
struct Ran {
    out: String,
    err: String,
    code: i32,
}

impl Ran {
    fn line(&self, key: &str) -> &str {
        self.out
            .lines()
            .find_map(|l| l.strip_prefix(key))
            .unwrap_or_else(|| panic!("no `{key}` line: {}", self.out))
    }
    fn count(&self, key: &str) -> u64 {
        let line = self.out.lines().find(|l| l.starts_with("turns:")).unwrap();
        let at = line
            .find(key)
            .unwrap_or_else(|| panic!("no {key} in {line}"));
        let tail = line[at + key.len()..].trim_start();
        let digits: String = tail.chars().take_while(char::is_ascii_digit).collect();
        digits.parse().unwrap()
    }
    fn status(&self) -> &str {
        self.line("status: ")
    }
}

fn run_mission(tag: &str, finish_at: usize, max_steps: u64) -> Ran {
    let dir = TestDir::new(&format!("tole-mission-cap-{tag}"));
    let base = spawn_mock(finish_at);
    let sessions = dir.join("sessions");
    let work = dir.join("work");
    std::fs::create_dir_all(&work).unwrap();
    let mut cmd = common::hermetic_command(
        env!("CARGO_BIN_EXE_tole"),
        &dir.join("home"),
        &[
            ("TOLE_BASE_URL", base.as_str()),
            ("TOLE_MODEL", "mock-model"),
            ("TOLE_API_KEY", "sk-test"),
        ],
    );
    cmd.args(["mission", "-s", sessions.to_str().unwrap(), "--yes"])
        .args(["--max-steps", &max_steps.to_string()])
        .arg("write the change")
        .current_dir(&work)
        .stdin(Stdio::null());
    let o = cmd.output().expect("run tole");
    Ran {
        out: String::from_utf8_lossy(&o.stdout).into(),
        err: String::from_utf8_lossy(&o.stderr).into(),
        code: o.status.code().unwrap_or(-1),
    }
}

/// AC1 + AC5: a first turn that uses all 32 steps does not end the mission;
/// the second turn completes it.
#[test]
fn a_first_turn_at_the_step_cap_is_followed_by_a_completing_turn() {
    let r = run_mission("continue", TURN_CAP, 120);
    assert_eq!(r.code, 0, "{}\n{}", r.out, r.err);
    assert_eq!(r.status(), "complete", "{}\n{}", r.out, r.err);
    assert_eq!(r.count("turns:"), 2, "{}", r.out);
    assert_eq!(r.count("steps:"), TURN_CAP as u64 + 1, "{}", r.out);
    assert!(r.err.contains("32-step cap (1/3)"), "{}", r.err);
}

/// AC2 + AC4: a model that never finishes a turn is stopped after
/// `TURN_CAP_TRIPS` capped turns in a row, with a status that names the cap
/// (not a mission budget), and the session stays a normal durable one.
#[test]
fn consecutive_capped_turns_stop_with_a_status_naming_the_turn_cap() {
    let r = run_mission("cap", usize::MAX, 200);
    assert_ne!(r.code, 0, "an unfinished mission exits nonzero: {}", r.out);
    assert_eq!(r.status(), "exhausted_turn_cap", "{}\n{}", r.out, r.err);
    assert_eq!(r.count("turns:"), TRIPS, "{}", r.out);
    assert_eq!(r.count("steps:"), TRIPS * TURN_CAP as u64, "{}", r.out);
    assert!(r.err.contains("32-step cap (3/3)"), "{}", r.err);
}

/// AC3: a mission step budget below the per-turn cap still ends the mission
/// (as the budget, not as the cap), after the one turn that spent it.
#[test]
fn a_mission_step_budget_below_the_turn_cap_still_ends_the_mission() {
    let r = run_mission("budget", usize::MAX, 10);
    assert_ne!(r.code, 0, "{}", r.out);
    assert_eq!(r.status(), "exhausted_steps", "{}\n{}", r.out, r.err);
    assert_eq!(r.count("turns:"), 1, "{}", r.out);
}
