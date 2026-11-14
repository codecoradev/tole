//! #457: the reasoning-effort setting end to end with the REAL binary and
//! a mock provider that echoes the `reasoning_effort` it was sent. The
//! session's durable `fact/reasoning_effort` register beats the process
//! setting (flag > env > config), survives `tole resume`, and a provider
//! that rejects the parameter still answers (one degraded retry).

use serde_json::{json, Value};
use std::io::{Read, Write};
use std::net::TcpListener;
use std::process::{Command, Stdio};
use std::time::Duration;
use tole_core::register::RegisterWrite;
use tole_core::storage::{Commit, JsonlStorage, Storage};

/// Answers `EFFORT::<value|none>`. With `reject`, a body carrying
/// `reasoning_effort` gets a 400 naming it (the retry must then drop it).
fn spawn_mock(reject: bool) -> String {
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
                let effort = body.get("reasoning_effort").and_then(Value::as_str);
                if reject && effort.is_some() {
                    let data = r#"{"error":{"message":"unknown parameter: reasoning_effort"}}"#;
                    let resp = format!(
                        "HTTP/1.1 400 Bad Request\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{data}",
                        data.len()
                    );
                    let _ = s.write_all(resp.as_bytes());
                    return;
                }
                let content = format!("EFFORT::{}", effort.unwrap_or("none"));
                let mut out = String::new();
                for part in [content.as_str(), ""] {
                    out.push_str(&format!(
                        "data: {}\n\n",
                        json!({"choices": [{"index": 0,
                            "delta": {"content": part}, "finish_reason": null}]})
                    ));
                }
                out.push_str("data: [DONE]\n\n");
                let resp = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    out.len(),
                    out
                );
                let _ = s.write_all(resp.as_bytes());
            });
        }
    });
    format!("http://{addr}/v1")
}

/// Serialises `fork`+`exec` of a child against the creation of a session
/// fixture. The session's writer lock is an `flock` on an open file
/// description, which a child forked by ANOTHER test thread duplicates until
/// it execs; the `tole` this test then spawns saw the lock held by the test
/// process ("session is open for writing by pid ...", #646). `spawn` returns
/// only once the child has exec'd, so while this mutex is held no such
/// duplicate exists.
static SPAWN: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn tole(url: &str, env: &[(&str, &str)], args: &[&str]) -> (String, String, i32) {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_tole"));
    cmd.args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .env("TOLE_BASE_URL", url)
        .env("TOLE_MODEL", "m")
        .env("TOLE_API_KEY", "k")
        .env("TOLE_NO_UPDATE_CHECK", "1")
        .env("PATH", "/usr/bin:/bin")
        .env_remove("TOLE_REASONING_EFFORT")
        .env_remove("TOLE_FALLBACK_BASE_URL")
        .env_remove("TOLE_MEMORY")
        .env_remove("TOLE_MEMORY_NAMESPACE")
        .env_remove("TOLE_SYSTEM_PROMPT")
        .env_remove("TOLE_TRUST")
        .env_remove("TOLE_AGENT_DEPTH");
    for (k, v) in env {
        cmd.env(k, v);
    }
    let mut child = {
        // #646: no fork may overlap a fixture's open session (see `session`).
        let _g = SPAWN.lock().unwrap_or_else(|p| p.into_inner());
        cmd.spawn().expect("run tole")
    };
    drop(child.stdin.take());
    let out = child.wait_with_output().unwrap();
    (
        String::from_utf8_lossy(&out.stdout).to_string(),
        String::from_utf8_lossy(&out.stderr).to_string(),
        out.status.code().unwrap_or(-1),
    )
}

/// A settled session, optionally with a `fact/reasoning_effort` register.
fn session(tag: &str, register: Option<&str>) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "tole-session-effort-{tag}-{}-{:x}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let _g = SPAWN.lock().unwrap_or_else(|p| p.into_inner());
    let mut s = JsonlStorage::create(&dir, "s-eff", None).unwrap();
    if let Some(level) = register {
        s.commit(Commit::new().register(RegisterWrite::set(
            "fact",
            "reasoning_effort",
            json!(level),
        )))
        .unwrap();
    }
    dir
}

fn resume(url: &str, env: &[(&str, &str)], dir: &std::path::Path) -> (String, String, i32) {
    tole(
        url,
        env,
        &["resume", "-s", dir.to_str().unwrap(), "s-eff", "go"],
    )
}

#[test]
fn unset_sends_nothing() {
    let url = spawn_mock(false);
    let dir = session("unset", None);
    let (out, err, code) = resume(&url, &[], &dir);
    assert_eq!(code, 0, "{err}");
    assert!(out.contains("EFFORT::none"), "{out}\n{err}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn register_round_trips_across_resume_and_beats_env() {
    let url = spawn_mock(false);
    let dir = session("reg", Some("high"));
    for env in [&[][..], &[("TOLE_REASONING_EFFORT", "low")][..]] {
        let (out, err, code) = resume(&url, env, &dir);
        assert_eq!(code, 0, "{err}");
        assert!(out.contains("EFFORT::high"), "{out}\n{err}");
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn env_and_flag_reach_the_wire_flag_wins() {
    let url = spawn_mock(false);
    let dir = session("env", None);
    let (out, err, code) = resume(&url, &[("TOLE_REASONING_EFFORT", "low")], &dir);
    assert_eq!(code, 0, "{err}");
    assert!(out.contains("EFFORT::low"), "{out}\n{err}");
    let (out, err, code) = tole(
        &url,
        &[("TOLE_REASONING_EFFORT", "low")],
        &[
            "--effort",
            "minimal",
            "resume",
            "-s",
            dir.to_str().unwrap(),
            "s-eff",
            "go",
        ],
    );
    assert_eq!(code, 0, "{err}");
    assert!(out.contains("EFFORT::minimal"), "{out}\n{err}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn invalid_levels_are_refused_loudly() {
    let url = spawn_mock(false);
    let dir = session("bad", None);
    let (_, err, code) = resume(&url, &[("TOLE_REASONING_EFFORT", "turbo")], &dir);
    assert_ne!(code, 0);
    assert!(err.contains("TOLE_REASONING_EFFORT"), "{err}");
    let (_, err, code) = tole(&url, &[], &["--effort", "turbo", "status", "x"]);
    assert_ne!(code, 0);
    assert!(err.contains("minimal | low | medium | high"), "{err}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_rejecting_provider_still_answers_without_the_parameter() {
    let url = spawn_mock(true);
    let dir = session("reject", Some("high"));
    let (out, err, code) = resume(&url, &[], &dir);
    assert_eq!(code, 0, "{err}");
    assert!(out.contains("EFFORT::none"), "{out}\n{err}");
    let _ = std::fs::remove_dir_all(&dir);
}

/// #646: children forked by other threads must not leave the fixture's
/// session locked for the `tole` that resumes it. Six threads keep
/// spawning `tole` while the main thread repeatedly writes a session and
/// resumes it; every resume must be answered.
#[test]
fn a_resume_is_never_refused_by_children_forked_by_other_threads() {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;

    let url = spawn_mock(false);
    let stop = Arc::new(AtomicBool::new(false));
    let spawners: Vec<_> = (0..6)
        .map(|_| {
            let (stop, url) = (stop.clone(), url.clone());
            std::thread::spawn(move || {
                while !stop.load(Ordering::Relaxed) {
                    let _ = tole(&url, &[], &["--version"]);
                }
            })
        })
        .collect();
    let mut failures = Vec::new();
    for i in 0..200 {
        let dir = session("fork-race", Some("high"));
        let (out, err, code) = resume(&url, &[], &dir);
        if code != 0 || !out.contains("EFFORT::high") {
            failures.push(format!("round {i}: code {code}: {err}"));
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
    stop.store(true, Ordering::Relaxed);
    for t in spawners {
        t.join().unwrap();
    }
    assert!(failures.is_empty(), "{failures:?}");
}
