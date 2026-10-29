//! #439: the global session index end to end with the REAL binary — a
//! session started in project A is listed by `tole sessions --all` from
//! project B, and `tole resume <id>` from B names the command to run in A.
//! `CODECORA_HOME` is isolated per test.

use serde_json::{json, Value};
use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

static N: AtomicU64 = AtomicU64::new(0);

fn tmpdir(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!(
        "tole-index-e2e-{tag}-{}-{}",
        std::process::id(),
        N.fetch_add(1, Ordering::SeqCst)
    ));
    std::fs::create_dir_all(&d).unwrap();
    d.canonicalize().unwrap()
}

/// Mock provider: every request gets the final answer `ok`.
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
                let resp = if body["stream"].as_bool() == Some(true) {
                    let mut out = String::new();
                    for part in ["ok", ""] {
                        out.push_str(&format!(
                            "data: {}\n\n",
                            json!({"choices": [{"index": 0,
                                "delta": {"content": part}, "finish_reason": null}]})
                        ));
                    }
                    out.push_str("data: [DONE]\n\n");
                    format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{out}",
                        out.len()
                    )
                } else {
                    let data = json!({
                        "id": "c", "object": "chat.completion", "created": 1, "model": "mock",
                        "choices": [{"index": 0, "message": {"role": "assistant",
                            "content": "ok"}, "finish_reason": "stop"}]
                    })
                    .to_string();
                    format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{data}",
                        data.len()
                    )
                };
                let _ = s.write_all(resp.as_bytes());
            });
        }
    });
    format!("http://{addr}/v1")
}

fn tole(cwd: &Path, home: &Path, url: &str, args: &[&str], extra: &[(&str, &str)]) -> Output {
    let mut c = Command::new(env!("CARGO_BIN_EXE_tole"));
    c.args(args)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .env("CODECORA_HOME", home)
        .env("TOLE_BASE_URL", url)
        .env("TOLE_MODEL", "m")
        .env("TOLE_API_KEY", "k")
        .env("TOLE_NO_UPDATE_CHECK", "1")
        .env("PATH", "/usr/bin:/bin")
        .env_remove("TOLE_NO_SESSION_INDEX")
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

fn session_id(out: &Output) -> String {
    text(&out.stdout)
        .lines()
        .find_map(|l| l.strip_prefix("session: "))
        .expect("run prints the session id")
        .trim()
        .to_string()
}

#[test]
fn a_session_is_listed_everywhere_and_resumed_in_its_own_project() {
    let url = spawn_mock();
    let home = tmpdir("home");
    let (a, b) = (tmpdir("project-a"), tmpdir("project-b"));
    let o = tole(
        &a,
        &home,
        &url,
        &["run", "summarize the release notes"],
        &[],
    );
    assert!(o.status.success(), "{}", text(&o.stderr));
    let id = session_id(&o);

    // Project B's own dir knows nothing; --all knows the session and where.
    let local = tole(&b, &home, &url, &["sessions"], &[]);
    assert!(!text(&local.stdout).contains(&id));
    let all = tole(&b, &home, &url, &["sessions", "--all"], &[]);
    let out = text(&all.stdout);
    assert!(all.status.success(), "{}", text(&all.stderr));
    assert!(out.contains(&id), "{out}");
    assert!(out.contains(&a.display().to_string()), "{out}");
    assert!(out.contains("summarize the release notes"), "{out}");
    let hit = tole(
        &b,
        &home,
        &url,
        &["sessions", "--all", "--grep", "RELEASE"],
        &[],
    );
    assert!(text(&hit.stdout).contains(&id));
    let miss = tole(
        &b,
        &home,
        &url,
        &["sessions", "--all", "--grep", "nope"],
        &[],
    );
    assert!(!text(&miss.stdout).contains(&id));

    // Resume by id from project B is refused (B's workspace, config and
    // trust would apply to A's session); the error names the command.
    let r = tole(&b, &home, &url, &["resume", &id, "and the changelog"], &[]);
    assert!(!r.status.success());
    let err = text(&r.stderr);
    assert!(err.contains("belongs to another project"), "{err}");
    assert!(
        err.contains(&format!("cd {} && tole resume {id}", a.display())),
        "{err}"
    );
    let file = a.join(format!(".tole/sessions/{id}.jsonl"));
    assert!(!std::fs::read_to_string(&file)
        .unwrap()
        .contains("and the changelog"));
    // Run where it says: it resumes.
    let r = tole(&a, &home, &url, &["resume", &id, "and the changelog"], &[]);
    assert!(r.status.success(), "{}", text(&r.stderr));
    assert!(std::fs::read_to_string(&file)
        .unwrap()
        .contains("and the changelog"));

    // The index never holds prompt text.
    let index = std::fs::read_to_string(home.join("tole/sessions-index.jsonl")).unwrap();
    assert!(!index.contains("release notes"), "{index}");
}

#[test]
fn the_index_can_be_turned_off() {
    let url = spawn_mock();
    let home = tmpdir("home-off");
    let a = tmpdir("project-off");
    let o = tole(
        &a,
        &home,
        &url,
        &["run", "hi"],
        &[("TOLE_NO_SESSION_INDEX", "1")],
    );
    assert!(o.status.success(), "{}", text(&o.stderr));
    assert!(!home.join("tole/sessions-index.jsonl").exists());
    let all = tole(&a, &home, &url, &["sessions", "--all"], &[]);
    assert!(text(&all.stdout).contains("no indexed sessions"));
    // --grep needs --all.
    let bad = tole(&a, &home, &url, &["sessions", "--grep", "x"], &[]);
    assert!(!bad.status.success());
}
