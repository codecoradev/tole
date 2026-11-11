//! #584 end-to-end with the REAL binary: `--profile <name>` applies a
//! dormant `[profiles.<name>]` table of the USER config to a session.
//!
//! Hermetic: `CODECORA_HOME` and the provider variables are set on the
//! child only, the provider is a local mock that records request bodies,
//! and `PATH` holds no cora / uteke / gh.

use serde_json::{json, Value};
use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

static COUNTER: AtomicU64 = AtomicU64::new(0);

fn tmpdir() -> PathBuf {
    let n = COUNTER.fetch_add(1, Ordering::SeqCst);
    let d = std::env::temp_dir().join(format!("tole-profile-e2e-{}-{n}", std::process::id()));
    std::fs::create_dir_all(&d).unwrap();
    d.canonicalize().unwrap()
}

fn text(b: &[u8]) -> String {
    String::from_utf8_lossy(b).to_string()
}

type Seen = Arc<Mutex<Vec<Value>>>;

/// A non-streaming mock that answers every request with a final message and
/// records each request body.
fn spawn_mock() -> (String, Seen) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let seen: Seen = Arc::default();
    let seen2 = Arc::clone(&seen);
    std::thread::spawn(move || {
        for mut s in listener.incoming().flatten() {
            let mut buf = Vec::new();
            let mut chunk = [0u8; 16384];
            let deadline = Instant::now() + Duration::from_secs(10);
            while Instant::now() < deadline {
                let n = s.read(&mut chunk).unwrap_or(0);
                buf.extend_from_slice(&chunk[..n]);
                if let Some(h) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                    if let Ok(v) = serde_json::from_slice::<Value>(&buf[h + 4..]) {
                        seen2.lock().unwrap().push(v);
                        break;
                    }
                }
                if n == 0 {
                    break;
                }
            }
            let data = json!({
                "id": "c", "object": "chat.completion", "created": 1, "model": "mock",
                "choices": [{"index": 0, "message": {"role": "assistant", "content": "done"},
                    "finish_reason": "stop"}],
                "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2}
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
    (format!("http://{addr}/v1"), seen)
}

/// A fresh home whose user config is `config`, plus files beside it.
fn home_with(config: &str, files: &[(&str, &str)]) -> PathBuf {
    let home = tmpdir();
    let dir = home.join("tole");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("config.toml"), config).unwrap();
    for (name, body) in files {
        std::fs::write(dir.join(name), body).unwrap();
    }
    home
}

fn cmd(cwd: &Path, home: &Path, url: &str, args: &[&str]) -> Command {
    let mut c = Command::new(env!("CARGO_BIN_EXE_tole"));
    c.args(args)
        .current_dir(cwd)
        .env("TOLE_NO_UPDATE_CHECK", "1")
        .env("CODECORA_HOME", home)
        .env("HOME", home)
        .env("PATH", format!("{}:/usr/bin:/bin", tmpdir().display()))
        .env("TOLE_STREAM", "0")
        .env("TOLE_BASE_URL", url)
        .env("TOLE_MODEL", "env-model")
        .env("TOLE_API_KEY", "k")
        .stdin(Stdio::null());
    for k in [
        "TOLE_SYSTEM_PROMPT",
        "TOLE_MEMORY",
        "TOLE_TRUST",
        "TOLE_SKILL",
        "TOLE_MEMORY_NAMESPACE",
        "OPENAI_API_KEY",
        "OPENAI_BASE_URL",
        "OPENAI_MODEL",
    ] {
        c.env_remove(k);
    }
    c
}

fn finish(mut c: Command) -> Output {
    c.stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = c.spawn().unwrap();
    let start = Instant::now();
    loop {
        if child.try_wait().unwrap().is_some() {
            return child.wait_with_output().unwrap();
        }
        if start.elapsed() > Duration::from_secs(60) {
            let _ = child.kill();
            panic!("child did not finish in 60s");
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

const CONFIG: &str = r#"
[profiles.coder]
system_prompt_file = "coder.md"
[profiles.coder.models]
light = "small-model"
standard = "profile-model"

[profiles.dormant]
system_prompt_file = "never-read.md"
"#;

#[test]
fn a_profile_sets_the_persona_and_beats_the_env_model() {
    let (url, seen) = spawn_mock();
    let home = home_with(CONFIG, &[("coder.md", "You are the coder persona.")]);
    let work = tmpdir();
    let o = finish(cmd(
        &work,
        &home,
        &url,
        &["--profile", "coder", "run", "hi"],
    ));
    assert!(o.status.success(), "{}", text(&o.stderr));
    let body = seen.lock().unwrap()[0].clone();
    assert_eq!(body["model"], "profile-model", "{body}");
    assert_eq!(
        body["messages"][0],
        json!({"role": "system", "content": "You are the coder persona."})
    );
    // `dormant` names a missing persona file: selecting `coder` never read it.
    // Without --profile the session is unchanged.
    let o = finish(cmd(&work, &home, &url, &["run", "hi"]));
    assert!(o.status.success(), "{}", text(&o.stderr));
    let body = seen.lock().unwrap()[1].clone();
    assert_eq!(body["model"], "env-model");
    assert_ne!(body["messages"][0]["content"], "You are the coder persona.");
}

#[test]
fn an_unknown_profile_is_a_startup_error_before_any_request() {
    let (url, seen) = spawn_mock();
    let home = home_with(CONFIG, &[("coder.md", "x")]);
    let o = finish(cmd(
        &tmpdir(),
        &home,
        &url,
        &["--profile", "writer", "run", "hi"],
    ));
    assert!(!o.status.success());
    let err = text(&o.stderr);
    assert!(
        err.contains("unknown profile `writer` (defined: coder, dormant)"),
        "{err}"
    );
    assert!(seen.lock().unwrap().is_empty(), "no request was made");
}

#[test]
fn a_project_config_cannot_define_profiles() {
    let (url, seen) = spawn_mock();
    let home = home_with("", &[]);
    let work = tmpdir();
    std::fs::create_dir_all(work.join(".tole")).unwrap();
    std::fs::write(
        work.join(".tole/config.toml"),
        "[profiles.evil]\nallow = [\"*\"]\n",
    )
    .unwrap();
    let trust = finish(cmd(&work, &home, &url, &["config", "trust", "--yes"]));
    assert!(trust.status.success(), "{}", text(&trust.stderr));
    let o = finish(cmd(&work, &home, &url, &["--profile", "evil", "run", "hi"]));
    assert!(!o.status.success());
    let err = text(&o.stderr);
    assert!(
        err.contains("ignoring `profiles` in project config"),
        "{err}"
    );
    assert!(err.contains("unknown profile `evil`"), "{err}");
    assert!(seen.lock().unwrap().is_empty());
}
