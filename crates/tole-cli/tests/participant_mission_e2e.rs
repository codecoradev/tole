//! An independent discussion participant is launched by its host as
//! `tole mission --plan-mode` with the participant marker (#704, #1039). The
//! real binary must run it, read-only, and must still refuse plan mode for
//! any other mission.

mod common;
use common::TestDir;
use serde_json::{json, Value};
use std::io::{Read, Write};
use std::net::TcpListener;
use std::process::Output;
use std::sync::mpsc;
use std::time::{Duration, Instant};

/// A provider that answers every request with text and reports the names of
/// the tools it was offered.
fn spawn_mock() -> (String, mpsc::Receiver<Vec<String>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        for mut s in listener.incoming().flatten() {
            let mut buf = Vec::new();
            let mut chunk = [0u8; 65536];
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
            let names = body["tools"]
                .as_array()
                .map(|t| {
                    t.iter()
                        .filter_map(|t| t["function"]["name"].as_str())
                        .map(str::to_string)
                        .collect()
                })
                .unwrap_or_default();
            let _ = tx.send(names);
            let data = json!({
                "id": "c", "object": "chat.completion", "created": 1, "model": "mock",
                "choices": [{"index": 0, "finish_reason": "stop",
                    "message": {"role": "assistant", "content": "done"}}],
                "usage": {"prompt_tokens": 10, "completion_tokens": 5, "total_tokens": 15}
            })
            .to_string();
            let _ = s.write_all(
                format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{data}",
                    data.len()
                )
                .as_bytes(),
            );
        }
    });
    (format!("http://{addr}/v1"), rx)
}

fn mission(home: &std::path::Path, base: &str, participant: bool) -> Output {
    let mut c = common::hermetic_command(env!("CARGO_BIN_EXE_tole"), home, &[]);
    c.args(["mission", "--max-steps", "2", "--plan-mode", "look it up"])
        .env("TOLE_NO_UPDATE_CHECK", "1")
        .env("TOLE_BASE_URL", base)
        .env("TOLE_MODEL", "mock")
        .env("TOLE_API_KEY", "k")
        .current_dir(home);
    if participant {
        c.env("TOLE_DISCUSSION_PARTICIPANT", "1");
    }
    c.output().unwrap()
}

#[test]
fn only_a_discussion_participant_may_run_a_mission_in_plan_mode_and_it_stays_read_only() {
    let home = TestDir::new("tole-participant-mission");
    let (base, tools) = spawn_mock();

    let other = mission(&home, &base, false);
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&other.stdout),
        String::from_utf8_lossy(&other.stderr)
    );
    assert!(!other.status.success(), "{text}");
    assert!(
        text.contains("--plan-mode has no meaning for a mission"),
        "{text}"
    );
    assert!(tools.try_recv().is_err(), "no model call was made");

    let part = mission(&home, &base, true);
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&part.stdout),
        String::from_utf8_lossy(&part.stderr)
    );
    assert!(!text.contains("has no meaning for a mission"), "{text}");
    let offered = tools.recv_timeout(Duration::from_secs(20)).expect(&text);
    assert!(offered.iter().any(|t| t == "read_file"), "{offered:?}");
    for write in [
        "write_file",
        "edit_file",
        "run_command",
        "delete_file",
        "git",
    ] {
        assert!(!offered.iter().any(|t| t == write), "{write}: {offered:?}");
    }
}
