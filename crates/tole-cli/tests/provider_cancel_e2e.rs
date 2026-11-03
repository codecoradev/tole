//! #484: a cancel (here the `--timeout` wall-clock cap, which fires the same
//! `CancelToken` as Esc / ACP `session/cancel`) must interrupt a provider
//! call that is already in flight, instead of waiting for the provider to
//! answer. Real `tole` binary against a mock that stays silent for a while.

use serde_json::json;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::process::Command;
use std::time::{Duration, Instant};

/// How long the slow mock stays silent before answering.
const SILENT: Duration = Duration::from_secs(8);

/// A provider that reads the request, waits `delay`, then answers `text`
/// (as an SSE stream when `stream`, else as one JSON body).
fn serve(text: &'static str, delay: Duration, stream: bool) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    std::thread::spawn(move || {
        for conn in listener.incoming() {
            let Ok(mut s) = conn else { continue };
            std::thread::spawn(move || {
                let _ = s.set_read_timeout(Some(Duration::from_secs(5)));
                let mut buf = Vec::new();
                let mut chunk = [0u8; 16384];
                loop {
                    let n = s.read(&mut chunk).unwrap_or(0);
                    if n == 0 {
                        return;
                    }
                    buf.extend_from_slice(&chunk[..n]);
                    if let Some(h) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                        let head = String::from_utf8_lossy(&buf[..h]).to_ascii_lowercase();
                        let len: usize = head
                            .lines()
                            .find_map(|l| l.strip_prefix("content-length:"))
                            .and_then(|v| v.trim().parse().ok())
                            .unwrap_or(0);
                        if buf.len() >= h + 4 + len {
                            break;
                        }
                    }
                }
                std::thread::sleep(delay);
                let reply = if stream {
                    let chunk = json!({"choices":[{"delta":{"content":text}}]});
                    let usage =
                        json!({"choices":[],"usage":{"prompt_tokens":2,"completion_tokens":1}});
                    let body = format!("data: {chunk}\n\ndata: {usage}\n\ndata: [DONE]\n\n");
                    format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    )
                } else {
                    let body = json!({"choices":[{"message":{"role":"assistant","content":text}}],
                                      "usage":{"prompt_tokens":2,"completion_tokens":1}})
                    .to_string();
                    format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    )
                };
                let _ = s.write_all(reply.as_bytes());
            });
        }
    });
    format!("http://{addr}/v1")
}

fn tmpdir(tag: &str) -> std::path::PathBuf {
    let d = std::env::temp_dir().join(format!("tole-cancel-e2e-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn tole(dir: &std::path::Path, base: &str, stream: bool, args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_tole"))
        .arg("--sessions-dir")
        .arg(dir.join("sessions"))
        .args(args)
        .current_dir(dir)
        .env("TOLE_BASE_URL", base)
        .env("TOLE_MODEL", "m")
        .env("TOLE_API_KEY", "sk-test")
        .env("TOLE_STREAM", if stream { "1" } else { "0" })
        .env("TOLE_NO_UPDATE_CHECK", "1")
        .env_remove("TOLE_MEMORY")
        .env_remove("TOLE_TRUST")
        .env_remove("OPENAI_BASE_URL")
        .env_remove("OPENAI_API_KEY")
        .env_remove("OPENAI_MODEL")
        .output()
        .unwrap()
}

fn cancel_while_silent(stream: bool) {
    let dir = tmpdir(if stream { "stream" } else { "plain" });
    let slow = serve("LATE", SILENT, stream);
    let t0 = Instant::now();
    let out = tole(&dir, &slow, stream, &["run", "hello", "--timeout", "1"]);
    let took = t0.elapsed();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(8), "stderr={stderr}");
    // Timeout 1 s + slice + process start; far below the 8 s the mock waits.
    assert!(took < Duration::from_secs(4), "cancel took {took:?}");

    // The session settled durably and stays resumable.
    let log_path = std::fs::read_dir(dir.join("sessions"))
        .unwrap()
        .flatten()
        .map(|e| e.path())
        .find(|p| p.extension().is_some_and(|x| x == "jsonl"))
        .expect("a session log");
    let log = std::fs::read_to_string(&log_path).unwrap();
    assert!(log.contains("cancelled"), "{log}");
    assert!(!log.contains("LATE"), "abandoned answer committed: {log}");
    let id = log_path.file_stem().unwrap().to_string_lossy().to_string();
    let fast = serve("RESUMED", Duration::ZERO, stream);
    let out = tole(&dir, &fast, stream, &["resume", &id, "continue"]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success(),
        "stdout={stdout}\nstderr={}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(stdout.contains("RESUMED"), "stdout={stdout}");
}

#[test]
fn timeout_interrupts_a_silent_provider_streaming() {
    cancel_while_silent(true);
}

#[test]
fn timeout_interrupts_a_silent_provider_non_stream() {
    cancel_while_silent(false);
}
