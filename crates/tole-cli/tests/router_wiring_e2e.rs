//! #586 part 2 end-to-end with the REAL binary: `[router]` picks the model
//! of a new `run`, `mission`, delegated mission and cron run, once, and
//! changes nothing else.
//!
//! Hermetic: `CODECORA_HOME`, `TOLE_CRON_DIR` and the provider variables
//! are set on the child only; the provider is a local mock that records
//! request bodies.

#![cfg(feature = "shell-tools")]

use serde_json::{json, Value};
use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::PathBuf;
use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

static COUNTER: AtomicU64 = AtomicU64::new(0);

fn tmpdir() -> PathBuf {
    let n = COUNTER.fetch_add(1, Ordering::SeqCst);
    let d = std::env::temp_dir().join(format!("tole-router-e2e-{}-{n}", std::process::id()));
    std::fs::create_dir_all(&d).unwrap();
    d.canonicalize().unwrap()
}

fn text(b: &[u8]) -> String {
    String::from_utf8_lossy(b).to_string()
}

type Seen = Arc<Mutex<Vec<Value>>>;

/// A non-streaming mock: every request gets a final "done" and is recorded.
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

struct Env {
    home: PathBuf,
    cron: PathBuf,
    work: PathBuf,
    url: String,
}

impl Env {
    fn new(url: &str, config: &str) -> Self {
        let root = tmpdir();
        let e = Env {
            home: root.join("home"),
            cron: root.join("cron"),
            work: root.join("work"),
            url: url.to_string(),
        };
        for d in [&e.home.join("tole"), &e.cron, &e.work] {
            std::fs::create_dir_all(d).unwrap();
        }
        std::fs::write(e.home.join("tole/config.toml"), config).unwrap();
        e
    }

    fn tole(&self, args: &[&str], extra: &[(&str, &str)]) -> Output {
        let mut c = Command::new(env!("CARGO_BIN_EXE_tole"));
        c.args(args)
            .current_dir(&self.work)
            .env("TOLE_NO_UPDATE_CHECK", "1")
            .env("CODECORA_HOME", &self.home)
            .env("HOME", &self.home)
            .env("TOLE_CRON_DIR", &self.cron)
            .env("PATH", "/usr/bin:/bin")
            .env("TOLE_STREAM", "0")
            .env("TOLE_BASE_URL", &self.url)
            .env("TOLE_MODEL", "env-model")
            .env("TOLE_API_KEY", "k")
            .env("TOLE_SKILL_APPROVAL", "off")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        for k in [
            "TOLE_SYSTEM_PROMPT",
            "TOLE_MEMORY",
            "TOLE_TRUST",
            "TOLE_SKILL",
            "TOLE_AGENT_DEPTH",
            "OPENAI_API_KEY",
            "OPENAI_BASE_URL",
            "OPENAI_MODEL",
        ] {
            c.env_remove(k);
        }
        for (k, v) in extra {
            c.env(k, v);
        }
        let mut child = c.spawn().unwrap();
        let start = Instant::now();
        loop {
            if child.try_wait().unwrap().is_some() {
                return child.wait_with_output().unwrap();
            }
            if start.elapsed() > Duration::from_secs(60) {
                let _ = child.kill();
                panic!("{args:?} did not finish in 60s");
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}

const CONFIG: &str = r#"
[router]
enabled = true
[router.tiers]
light = "small-model"
heavy = "big-model"
[[router.rules]]
name = "digest"
tier = "light"
kinds = ["run"]
keywords = ["digest"]
[[router.rules]]
name = "missions"
tier = "heavy"
kinds = ["mission"]
[[router.rules]]
name = "children"
tier = "light"
kinds = ["delegate"]
[[router.rules]]
name = "cron-digest"
tier = "light"
kinds = ["cron"]
jobs = ["digest-job", "pinned"]
"#;

fn last(seen: &Seen) -> Value {
    seen.lock()
        .unwrap()
        .last()
        .cloned()
        .expect("a provider request")
}

fn tool_names(body: &Value) -> Vec<String> {
    let mut v: Vec<String> = body["tools"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|t| t["function"]["name"].as_str().map(str::to_string))
        .collect();
    v.sort();
    v
}

#[test]
fn a_run_is_routed_once_and_keeps_its_tools() {
    let (url, seen) = spawn_mock();
    let e = Env::new(&url, CONFIG);
    let o = e.tole(&["run", "digest the inbox"], &[]);
    assert!(o.status.success(), "{}", text(&o.stderr));
    assert!(
        text(&o.stderr).contains("router: light -> small-model"),
        "{}",
        text(&o.stderr)
    );
    let routed = last(&seen);
    assert_eq!(routed["model"], "small-model", "{routed}");
    // No rule matches: the configured model, and the same tools.
    let o = e.tole(&["run", "hello"], &[]);
    assert!(o.status.success(), "{}", text(&o.stderr));
    let plain = last(&seen);
    assert_eq!(plain["model"], "env-model");
    assert_eq!(tool_names(&routed), tool_names(&plain));
    // Routing off: nothing changes.
    let off = Env::new(&url, &CONFIG.replace("enabled = true", "enabled = false"));
    let o = off.tole(&["run", "digest the inbox"], &[]);
    assert!(o.status.success(), "{}", text(&o.stderr));
    assert_eq!(last(&seen)["model"], "env-model");
}

#[test]
fn a_broken_router_stops_the_run_before_any_provider_call() {
    let (url, seen) = spawn_mock();
    let e = Env::new(&url, &CONFIG.replace("heavy = \"big-model\"\n", ""));
    let o = e.tole(&["run", "digest the inbox"], &[]);
    assert!(!o.status.success());
    assert!(
        text(&o.stderr).contains("[router.tiers]"),
        "{}",
        text(&o.stderr)
    );
    assert!(seen.lock().unwrap().is_empty(), "no request was sent");
    // The check runs before the session file exists: no stray session.
    let mut stray = Vec::new();
    jsonl_files(&e.home, &mut stray);
    jsonl_files(&e.work, &mut stray);
    assert!(stray.is_empty(), "{stray:?}");
}

fn jsonl_files(dir: &std::path::Path, out: &mut Vec<PathBuf>) {
    for entry in std::fs::read_dir(dir).into_iter().flatten().flatten() {
        let p = entry.path();
        if p.is_dir() {
            jsonl_files(&p, out);
        } else if p.extension().is_some_and(|x| x == "jsonl") {
            out.push(p);
        }
    }
}

#[test]
fn missions_and_delegated_children_route_by_kind() {
    let (url, seen) = spawn_mock();
    let e = Env::new(&url, CONFIG);
    let _ = e.tole(&["mission", "--max-steps", "1", "ship it"], &[]);
    assert_eq!(last(&seen)["model"], "big-model");
    // A delegated child is a mission with TOLE_AGENT_DEPTH >= 1.
    let _ = e.tole(
        &["mission", "--max-steps", "1", "ship it"],
        &[("TOLE_AGENT_DEPTH", "1")],
    );
    assert_eq!(last(&seen)["model"], "small-model");
}

#[test]
fn a_cron_run_is_routed_unless_the_job_pins_a_model() {
    let (url, seen) = spawn_mock();
    let e = Env::new(&url, CONFIG);
    for args in [
        vec!["cron", "create", "@daily", "say hi", "--id", "digest-job"],
        vec![
            "cron",
            "create",
            "@daily",
            "say hi",
            "--id",
            "pinned",
            "--model",
            "pin-model",
        ],
        vec!["cron", "create", "@daily", "say hi", "--id", "other"],
    ] {
        let o = e.tole(&args, &[]);
        assert!(o.status.success(), "{args:?}: {}", text(&o.stderr));
    }
    for (job, want) in [
        ("digest-job", "small-model"),
        ("other", "env-model"),
        // The rule matches `pinned` too, but its --model pin wins.
        ("pinned", "pin-model"),
    ] {
        let o = e.tole(&["cron", "run", job], &[]);
        assert!(o.status.success(), "{job}: {}", text(&o.stderr));
        assert_eq!(last(&seen)["model"], want, "{job}");
    }
}
