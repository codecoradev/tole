//! #945 model routing (#586) over the REAL `tole serve` face: a server
//! session is routed once, at its FIRST prompt, from the OWNER's `[router]`
//! config and exactly with the CLI's semantics (off by default; an explicit
//! model, a client-set one included, is never overridden; a resumed session
//! is never re-routed; a broken router fails the prompt before any provider
//! call; only `fact/model` and `fact/route` change).
//!
//! Hermetic: `CODECORA_HOME` and `HOME` are the test's, the provider is a
//! recording mock.

#![cfg(feature = "shell-tools")]

mod common;
use common::TestDir;
use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tole_core::storage::Storage;

type Seen = Arc<Mutex<Vec<Value>>>;

/// Non-streaming provider mock: records each request body; a conversation
/// mentioning TOOL_MISSION asks once for `write_file approved.txt`.
fn spawn_mock() -> (String, Seen) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let seen: Seen = Arc::default();
    let seen2 = Arc::clone(&seen);
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let seen = Arc::clone(&seen2);
            std::thread::spawn(move || {
                let mut s = stream;
                let mut buf = Vec::new();
                let mut chunk = [0u8; 16384];
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
                seen.lock().unwrap().push(body.clone());
                let msgs = body["messages"].as_array().cloned().unwrap_or_default();
                let last_user = msgs.iter().rposition(|m| m["role"] == "user");
                let has_result = last_user
                    .map(|i| msgs[i + 1..].iter().any(|m| m["role"] == "tool"))
                    .unwrap_or(false);
                let wants_tool = !has_result && body.to_string().contains("TOOL_MISSION");
                let message = if wants_tool {
                    json!({"role": "assistant", "content": null, "tool_calls": [{
                        "id": "call_1", "type": "function", "function": {
                            "name": "write_file",
                            "arguments": "{\"path\":\"approved.txt\",\"content\":\"ok\"}"}}]})
                } else {
                    json!({"role": "assistant", "content": "done"})
                };
                let data = json!({
                    "id": "c", "object": "chat.completion", "created": 1, "model": "mock",
                    "choices": [{"index": 0, "message": message,
                        "finish_reason": if wants_tool { "tool_calls" } else { "stop" }}],
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
            });
        }
    });
    (format!("http://{addr}/v1"), seen)
}

const ROUTED: &str = r#"
[router]
enabled = true
[router.tiers]
light = "small-model"
heavy = "big-model"
[[router.rules]]
name = "digest"
tier = "light"
keywords = ["digest"]
[[router.rules]]
name = "big"
tier = "heavy"
keywords = ["bigjob"]
"#;

struct Daemon {
    child: Child,
    port: u16,
    work: TestDir,
    home: TestDir,
    url: String,
    seen: Seen,
}

impl Daemon {
    fn spawn(config: &str) -> Self {
        let (url, seen) = spawn_mock();
        let home = TestDir::new("tole-route-serve-home");
        let work = TestDir::new("tole-route-serve-work");
        let dir = home.join("tole");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("config.toml"), config).unwrap();
        let mut d = Self {
            child: Command::new("true").spawn().unwrap(),
            port: 0,
            work,
            home,
            url,
            seen,
        };
        d.start();
        d
    }

    fn start(&mut self) {
        let (child, port) = common::spawn_listening(|port| {
            let mut cmd = Command::new(env!("CARGO_BIN_EXE_tole"));
            cmd.args(["serve", "--port", &port.to_string(), "--token", "t0ken"])
                .args(["--workspace", self.work.to_str().unwrap()])
                .current_dir(self.work.path())
                .env("TOLE_NO_UPDATE_CHECK", "1")
                .env("CODECORA_HOME", self.home.path())
                .env("HOME", self.home.path())
                .env("PATH", "/usr/bin:/bin")
                .env("TOLE_STREAM", "0")
                .env("TOLE_BASE_URL", &self.url)
                .env("TOLE_MODEL", "env-model")
                .env("TOLE_MODELS", "client-model")
                .env("TOLE_API_KEY", "sk-route-secret-0123456789")
                .stdin(Stdio::null())
                .stdout(Stdio::null());
            for k in [
                "TOLE_SYSTEM_PROMPT",
                "TOLE_MEMORY",
                "TOLE_TRUST",
                "TOLE_PROFILE",
            ] {
                cmd.env_remove(k);
            }
            cmd
        });
        self.child = child;
        self.port = port;
    }

    fn restart(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        self.start();
    }

    fn http(&self, method: &str, path: &str, body: Option<&str>) -> (u16, Value) {
        let mut s = std::net::TcpStream::connect(("127.0.0.1", self.port)).unwrap();
        let req = match body {
            Some(b) => format!(
                "{method} {path} HTTP/1.1\r\nAuthorization: Bearer t0ken\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{b}",
                b.len()
            ),
            None => format!(
                "{method} {path} HTTP/1.1\r\nAuthorization: Bearer t0ken\r\nConnection: close\r\n\r\n"
            ),
        };
        s.write_all(req.as_bytes()).unwrap();
        let mut reader = BufReader::new(s);
        let mut status = String::new();
        reader.read_line(&mut status).unwrap();
        let code = status
            .split_whitespace()
            .nth(1)
            .and_then(|c| c.parse().ok())
            .unwrap_or(0);
        let mut payload = String::new();
        loop {
            let mut line = String::new();
            if reader.read_line(&mut line).unwrap() == 0 {
                break;
            }
            if line == "\r\n" || line == "\n" {
                reader.read_to_string(&mut payload).unwrap();
                break;
            }
        }
        (
            code,
            serde_json::from_str(payload.trim()).unwrap_or(Value::Null),
        )
    }

    fn create(&self, body: Value) -> String {
        let (code, b) = self.http("POST", "/sessions", Some(&body.to_string()));
        assert_eq!(code, 200, "{b}");
        b["sessionId"].as_str().unwrap().to_string()
    }

    fn new_session(&self) -> String {
        self.create(json!({"cwd": self.work.to_str().unwrap()}))
    }

    fn prompt_raw(&self, id: &str, text: &str) -> (u16, Value) {
        self.http(
            "POST",
            &format!("/sessions/{id}/prompt"),
            Some(&json!({"text": text}).to_string()),
        )
    }

    fn prompt(&self, id: &str, text: &str) -> Value {
        let (code, b) = self.prompt_raw(id, text);
        assert_eq!(code, 200, "{b}");
        b
    }

    fn last_request(&self) -> Value {
        self.seen.lock().unwrap().last().cloned().unwrap()
    }

    fn requests(&self) -> usize {
        self.seen.lock().unwrap().len()
    }

    fn registers(&self, id: &str) -> (Option<Value>, Option<Value>) {
        let p = self.work.join(".tole/sessions").join(format!("{id}.jsonl"));
        let s = tole_core::storage::JsonlStorage::open_read_only(p).unwrap();
        (
            s.get_register("fact", "model").cloned(),
            s.get_register("fact", "route").cloned(),
        )
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
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
fn routing_is_off_unless_the_owner_enables_it() {
    let d = Daemon::spawn(&ROUTED.replace("enabled = true", "enabled = false"));
    let id = d.new_session();
    d.prompt(&id, "digest the inbox");
    assert_eq!(d.last_request()["model"], "env-model");
    assert_eq!(d.registers(&id), (None, None));
    // No config at all: the same.
    let d = Daemon::spawn("");
    let id = d.new_session();
    d.prompt(&id, "digest the inbox");
    assert_eq!(d.last_request()["model"], "env-model");
    assert_eq!(d.registers(&id), (None, None));
}

#[test]
fn the_first_prompt_is_routed_once_and_keeps_its_tools() {
    let d = Daemon::spawn(ROUTED);
    let id = d.new_session();
    d.prompt(&id, "digest the inbox");
    let routed = d.last_request();
    assert_eq!(routed["model"], "small-model", "{routed}");
    let (model, route) = d.registers(&id);
    assert_eq!(model, Some(json!("small-model")));
    let route = route.expect("fact/route recorded");
    assert_eq!(route["tier"], "light", "{route}");
    // A second prompt keeps the model and does not re-route: a prompt that
    // would now match no rule still runs on the routed model.
    d.prompt(&id, "something unrelated");
    assert_eq!(d.last_request()["model"], "small-model");
    assert_eq!(d.registers(&id).1, Some(route));
    // A session whose first prompt matches no rule: the configured model and
    // the SAME tools as the routed one.
    let other = d.new_session();
    d.prompt(&other, "hello");
    let plain = d.last_request();
    assert_eq!(plain["model"], "env-model");
    assert_eq!(tool_names(&routed), tool_names(&plain));
}

#[test]
fn a_client_set_model_is_never_overridden() {
    let d = Daemon::spawn(ROUTED);
    let id = d.new_session();
    let (code, b) = d.http(
        "POST",
        &format!("/sessions/{id}/model"),
        Some(&json!({"model": "client-model"}).to_string()),
    );
    assert_eq!(code, 200, "{b}");
    d.prompt(&id, "digest the inbox");
    assert_eq!(d.last_request()["model"], "client-model");
    let (model, route) = d.registers(&id);
    assert_eq!(model, Some(json!("client-model")));
    assert_eq!(route, None, "an explicit model is not routed");
}

#[test]
fn a_resumed_session_is_not_routed_again() {
    let mut d = Daemon::spawn(&ROUTED.replace("enabled = true", "enabled = false"));
    let id = d.new_session();
    d.prompt(&id, "digest the inbox");
    assert_eq!(d.registers(&id), (None, None));
    // The owner turns routing on; a restart and a resume of the old session.
    std::fs::write(d.home.join("tole/config.toml"), ROUTED).unwrap();
    d.restart();
    let (code, b) = d.http(
        "POST",
        "/sessions",
        Some(&json!({"cwd": d.work.to_str().unwrap(), "resume": id}).to_string()),
    );
    assert_eq!(code, 200, "{b}");
    d.prompt(&id, "digest again");
    assert_eq!(d.last_request()["model"], "env-model");
    assert_eq!(d.registers(&id), (None, None));
}

#[test]
fn a_broken_router_fails_the_prompt_before_any_provider_call() {
    let d = Daemon::spawn(&ROUTED.replace("heavy = \"big-model\"\n", ""));
    let id = d.new_session();
    let before = d.requests();
    let (code, b) = d.prompt_raw(&id, "digest the inbox");
    assert!(code >= 400, "{code} {b}");
    assert!(b.to_string().contains("[router"), "{b}");
    assert!(!b.to_string().contains("sk-route-secret-0123456789"), "{b}");
    assert_eq!(d.requests(), before, "no request was sent");
}

#[test]
fn a_router_error_is_redacted() {
    // The config is read again at the first prompt. When it no longer loads,
    // its error echoes what the owner wrote; here that is the provider key,
    // which must not reach the client.
    let d = Daemon::spawn(ROUTED);
    let id = d.new_session();
    std::fs::write(
        d.home.join("tole/config.toml"),
        ROUTED.replace("tier = \"heavy\"", "tier = \"sk-test\""),
    )
    .unwrap();
    let (code, b) = d.prompt_raw(&id, "digest the inbox");
    assert!(code >= 400, "{code} {b}");
    assert!(b.to_string().contains("unknown tier"), "{b}");
    assert!(!b.to_string().contains("sk-route-secret-0123456789"), "{b}");
    assert_eq!(d.requests(), 0);
}
