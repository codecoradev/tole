//! #475: two REAL `tole` processes on one session. While `chat --resume`
//! holds the session, `resume` from another process is refused naming the
//! holder (no duplicated seq in the file), read-only commands still work,
//! and once the holder exits the session resumes normally.

use serde_json::{json, Value};
use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

fn spawn_mock() -> String {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            std::thread::spawn(move || {
                let mut s = stream;
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
                let usage = json!({"prompt_tokens": 5, "completion_tokens": 1, "total_tokens": 6});
                let (ctype, data) = if body["stream"].as_bool() == Some(true) {
                    let mut out = String::new();
                    for part in ["ok", ""] {
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
                            "content": "ok"}, "finish_reason": "stop"}],
                        "usage": usage
                    });
                    ("application/json", data.to_string())
                };
                let resp = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: {ctype}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{data}",
                    data.len()
                );
                let _ = s.write_all(resp.as_bytes());
            });
        }
    });
    format!("http://{addr}/v1")
}

fn cmd(cwd: &Path, url: &str, args: &[&str]) -> Command {
    let mut c = Command::new(env!("CARGO_BIN_EXE_tole"));
    c.args(args)
        .current_dir(cwd)
        .env("CODECORA_HOME", cwd.join("home"))
        .env("TOLE_BASE_URL", url)
        .env("TOLE_MODEL", "m")
        .env("TOLE_API_KEY", "k")
        .env("TOLE_NO_UPDATE_CHECK", "1")
        .env("TOLE_NO_SESSION_INDEX", "1")
        .env("PATH", "/usr/bin:/bin");
    c
}

fn run(cwd: &Path, url: &str, args: &[&str]) -> Output {
    cmd(cwd, url, args).stdin(Stdio::null()).output().unwrap()
}

fn text(b: &[u8]) -> String {
    String::from_utf8_lossy(b).to_string()
}

fn seqs(file: &Path) -> Vec<u64> {
    let mut out = Vec::new();
    for l in std::fs::read_to_string(file).unwrap().lines().skip(1) {
        let v: Value = serde_json::from_str(l).unwrap();
        match v {
            Value::Array(a) => out.extend(a.iter().map(|r| r["seq"].as_u64().unwrap())),
            r => out.push(r["seq"].as_u64().unwrap()),
        }
    }
    out
}

#[test]
fn second_process_is_refused_and_the_file_stays_valid() {
    let url = spawn_mock();
    let d: PathBuf = std::env::temp_dir().join(format!("tole-lock-e2e-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    let o = run(&d, &url, &["run", "first turn"]);
    assert!(o.status.success(), "{}", text(&o.stderr));
    let id = text(&o.stdout)
        .lines()
        .find_map(|l| l.strip_prefix("session: ").map(str::to_string))
        .unwrap();
    let file = d.join(format!(".tole/sessions/{id}.jsonl"));
    let base = seqs(&file).len();

    // Process A: chat --resume, one message sent, then idles on stdin.
    let mut a = cmd(&d, &url, &["chat", "--resume", &id])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut a_in = a.stdin.take().unwrap();
    writeln!(a_in, "from A, message 1").unwrap();
    let deadline = Instant::now() + Duration::from_secs(20);
    while seqs(&file).len() <= base {
        assert!(Instant::now() < deadline, "A never wrote");
        std::thread::sleep(Duration::from_millis(50));
    }
    std::thread::sleep(Duration::from_millis(300));
    let before = std::fs::read(&file).unwrap();

    // Process B: refused, naming A; file untouched.
    let o = run(&d, &url, &["resume", &id, "from B"]);
    assert!(!o.status.success());
    let err = text(&o.stderr);
    assert!(err.contains(&format!("pid {}", a.id())), "{err}");
    assert_eq!(std::fs::read(&file).unwrap(), before);

    // Read-only commands work while A holds the lock.
    assert!(run(&d, &url, &["status", &id]).status.success());
    assert!(run(&d, &url, &["sessions"]).status.success());

    // A exits; the session is intact and resumes.
    drop(a_in);
    a.wait().unwrap();
    let all = seqs(&file);
    assert!(all.windows(2).all(|w| w[0] < w[1]), "dup seq: {all:?}");
    let o = run(&d, &url, &["resume", &id, "from B"]);
    assert!(o.status.success(), "{}", text(&o.stderr));
    assert!(run(&d, &url, &["status", &id]).status.success());
}
