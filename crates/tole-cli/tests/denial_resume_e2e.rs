//! #476, real binary + scripted mock: after a denied approval (or a
//! `--on-pretool` exit 2) `resume <id> "<prompt>"` runs a new turn whose
//! request carries the denial, instead of being refused with rc 1.

use serde_json::{json, Value};
use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// First request: a `write_file` call. Every later one: a final answer.
/// All request bodies are recorded.
fn spawn_mock(reqs: Arc<Mutex<Vec<Value>>>) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let reqs = reqs.clone();
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
                let first = {
                    let mut r = reqs.lock().unwrap();
                    r.push(body.clone());
                    r.len() == 1
                };
                let msg = if first {
                    json!({"role": "assistant", "content": null,
                        "tool_calls": [{"id": "c1", "type": "function",
                            "function": {"name": "write_file",
                                "arguments": json!({"path": "out.txt", "content": "x"}).to_string()}}]})
                } else {
                    json!({"role": "assistant", "content": "RESUMED-OK"})
                };
                let usage = json!({"prompt_tokens": 5, "completion_tokens": 1, "total_tokens": 6});
                let (ctype, data) = if body["stream"].as_bool() == Some(true) {
                    let delta = if first {
                        json!({"tool_calls": [{"index": 0, "id": "c1", "function": {
                            "name": "write_file",
                            "arguments": json!({"path": "out.txt", "content": "x"}).to_string()}}]})
                    } else {
                        json!({"content": "RESUMED-OK"})
                    };
                    let mut out = format!(
                        "data: {}\n\n",
                        json!({"choices": [{"index": 0, "delta": delta, "finish_reason": null}]})
                    );
                    out.push_str(&format!(
                        "data: {}\n\n",
                        json!({"choices": [], "usage": usage})
                    ));
                    out.push_str("data: [DONE]\n\n");
                    ("text/event-stream", out)
                } else {
                    let data = json!({
                        "id": "c", "object": "chat.completion", "created": 1, "model": "m",
                        "choices": [{"index": 0, "message": msg,
                            "finish_reason": if first { "tool_calls" } else { "stop" }}],
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

fn run(cwd: &Path, url: &str, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_tole"))
        .args(args)
        .current_dir(cwd)
        // Outside the workspace: a jail that holds the data dir has no file
        // writers (#684).
        .env("CODECORA_HOME", cwd.with_extension("home"))
        .env("TOLE_BASE_URL", url)
        .env("TOLE_MODEL", "m")
        .env("TOLE_API_KEY", "k")
        .env("TOLE_NO_UPDATE_CHECK", "1")
        .env("TOLE_NO_SESSION_INDEX", "1")
        .env("PATH", "/usr/bin:/bin")
        .stdin(Stdio::null())
        .output()
        .unwrap()
}

fn text(b: &[u8]) -> String {
    String::from_utf8_lossy(b).to_string()
}

fn workdir(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("tole-denial-e2e-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn session_id(cwd: &Path) -> String {
    std::fs::read_dir(cwd.join(".tole/sessions"))
        .unwrap()
        .flatten()
        .find_map(|e| {
            let n = e.file_name().to_string_lossy().to_string();
            n.strip_suffix(".jsonl").map(str::to_string)
        })
        .expect("a session file")
}

/// A fresh mock + session dir, then the first (denying) run. Marker that the
/// run really parked at a denial: `tole: approval denied for 'write_file'`
/// on stderr with rc 2. Anything else (rc 0 with the write executed, say a
/// transient hook spawn failure, which is non-blocking by contract) is
/// retried up to 3 times from a clean slate; the last attempt asserts with
/// full diagnostics. Retries are logged so they stay visible.
fn denied_first_run(
    tag: &str,
    first_args: &dyn Fn(&Path) -> Vec<String>,
) -> (Arc<Mutex<Vec<Value>>>, String, PathBuf) {
    for attempt in 1..=3 {
        let reqs = Arc::new(Mutex::new(Vec::new()));
        let url = spawn_mock(reqs.clone());
        let d = workdir(tag);
        let args = first_args(&d);
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        let o = run(&d, &url, &args);
        let err = text(&o.stderr);
        if o.status.code() == Some(2) && err.contains("approval denied for 'write_file'") {
            assert!(!d.join("out.txt").exists(), "a denied write must not run");
            assert_eq!(reqs.lock().unwrap().len(), 1);
            return (reqs, url, d);
        }
        let diag = format!(
            "attempt {attempt}: rc={:?} out.txt={} reqs={} stderr={err}",
            o.status.code(),
            d.join("out.txt").exists(),
            reqs.lock().unwrap().len()
        );
        assert!(attempt < 3, "first run never denied: {diag}");
        eprintln!("retrying first run — {diag}");
        let _ = std::fs::remove_dir_all(&d);
    }
    unreachable!()
}

/// The denied first run, then `resume <id> "<prompt>"`: rc 0, one new
/// request, and its user messages are [prompt-1, denial note, prompt-2].
fn deny_then_resume_with_prompt(
    tag: &str,
    first_args: &dyn Fn(&Path) -> Vec<String>,
    denial_marker: &str,
) {
    let (reqs, url, d) = denied_first_run(tag, first_args);

    let id = session_id(&d);
    let o = run(
        &d,
        &url,
        &["resume", &id, "please continue without that tool"],
    );
    assert_eq!(o.status.code(), Some(0), "{}", text(&o.stderr));
    assert!(
        text(&o.stdout).contains("RESUMED-OK"),
        "{}",
        text(&o.stdout)
    );
    assert!(!d.join("out.txt").exists());

    let reqs = reqs.lock().unwrap();
    assert_eq!(reqs.len(), 2, "exactly one new provider request");
    let users: Vec<String> = reqs[1]["messages"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|m| m["role"] == "user")
        .map(|m| m["content"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(users.len(), 3, "{users:?}");
    assert!(
        users[1].contains("write_file") && users[1].contains(denial_marker),
        "the wire must carry the denial: {users:?}"
    );
    assert_eq!(users[2], "please continue without that tool");
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn resume_with_a_prompt_after_an_approval_denial_shows_the_model_the_denial() {
    deny_then_resume_with_prompt(
        "approval",
        &|_| vec!["run".into(), "go".into()],
        "approval refused",
    );
}

#[cfg(feature = "shell-tools")]
#[test]
fn resume_with_a_prompt_after_a_pretool_hook_denial_shows_the_model_the_denial() {
    // The script lives in the session cwd, created per attempt and made
    // durable (file + directory entry synced, then read back) before `run`
    // so nothing races the write -> read window.
    let write_hook = |d: &Path| {
        let script = d.join("deny.sh");
        let body = "#!/bin/sh\necho policy says no\nexit 2\n";
        let mut f = std::fs::File::create(&script).unwrap();
        f.write_all(body.as_bytes()).unwrap();
        f.sync_all().unwrap();
        drop(f);
        std::fs::File::open(d).unwrap().sync_all().unwrap();
        assert_eq!(std::fs::read_to_string(&script).unwrap(), body);
        vec![
            "run".to_string(),
            "--yes".into(),
            "--on-pretool".into(),
            format!("/bin/sh {}", script.display()),
            "go".into(),
        ]
    };
    deny_then_resume_with_prompt("hook", &write_hook, "policy says no");
}

/// Builds without shell-tools cannot wire hooks: the flag must fail loudly,
/// never be silently ignored (the write would run under --yes).
#[cfg(not(feature = "shell-tools"))]
#[test]
fn bare_build_refuses_hook_flags_loudly() {
    let reqs = Arc::new(Mutex::new(Vec::new()));
    let url = spawn_mock(reqs.clone());
    let d = workdir("bare-refuse");
    let o = run(
        &d,
        &url,
        &["run", "--yes", "--on-pretool", "/bin/true", "go"],
    );
    assert_ne!(o.status.code(), Some(0));
    assert!(
        text(&o.stderr).contains("shell-tools"),
        "{}",
        text(&o.stderr)
    );
    assert!(!d.join("out.txt").exists());
    assert!(reqs.lock().unwrap().is_empty());
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn run_help_describes_the_resume_behaviour() {
    let o = Command::new(env!("CARGO_BIN_EXE_tole"))
        .args(["run", "--help"])
        .output()
        .unwrap();
    let help = text(&o.stdout).replace('\n', " ");
    let help = help.split_whitespace().collect::<Vec<_>>().join(" ");
    assert!(
        help.contains("settles the parked turn and runs the new prompt"),
        "{help}"
    );
}
