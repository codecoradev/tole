//! #863: `tole cron run` and `tole cron tick` EXECUTE jobs in this process,
//! so the owner's USER config (`[sandbox]`, `[sandbox.consumers]`, `[secrets]`,
//! `[sensitive_files]`) must apply to them like it does to `tole run`. The
//! other cron subcommands only edit files and keep working when the config is
//! broken. A project config is never read for cron (the job's workdir is not
//! trusted to configure the host).
//!
//! Real binary, hermetic: `CODECORA_HOME`, `TOLE_CRON_DIR` and the provider
//! variables are set on the child only; the provider is a local mock that asks
//! for one `run_command` and records what it was told afterwards. The child's
//! PATH has no `bubblewrap`, so a required sandbox cannot start.
#![cfg(all(unix, feature = "shell-tools"))]

mod common;
use common::TestDir;
use serde_json::{json, Value};
use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::PathBuf;
use std::process::{Command, Output, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

type Seen = Arc<Mutex<Vec<Value>>>;

fn text(b: &[u8]) -> String {
    String::from_utf8_lossy(b).to_string()
}

fn completion(message: Value) -> Value {
    json!({"id": "c", "object": "chat.completion", "created": 1, "model": "mock",
        "choices": [{"index": 0, "message": message,
            "finish_reason": if message["tool_calls"].is_array() { "tool_calls" } else { "stop" }}],
        "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2}})
}

/// A prompt with RUN asks for `run_command touch ran-marker` first; anything
/// else (and the turn after the tool result) gets a final answer.
fn reply(body: &Value) -> Value {
    let msgs = body["messages"].as_array().cloned().unwrap_or_default();
    let wants_run = msgs
        .iter()
        .any(|m| m["role"] == "user" && m["content"].as_str().unwrap_or("").contains("RUN"));
    let after_tool = msgs.last().is_some_and(|m| m["role"] == "tool");
    if wants_run && !after_tool {
        completion(
            json!({"role": "assistant", "content": null, "tool_calls": [{"id": "call_1",
            "type": "function", "function": {"name": "run_command",
            "arguments": json!({"command": "touch ran-marker"}).to_string()}}]}),
        )
    } else {
        completion(json!({"role": "assistant", "content": "done"}))
    }
}

fn spawn_mock() -> (String, Seen) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let seen: Seen = Arc::default();
    let seen2 = Arc::clone(&seen);
    std::thread::spawn(move || {
        for mut s in listener.incoming().flatten() {
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
            let data = reply(&body).to_string();
            seen2.lock().unwrap().push(body);
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
    bin: PathBuf,
    url: String,
    _root: TestDir,
}

impl Env {
    fn new(url: &str) -> Self {
        let root = TestDir::new("tole-cron-ucfg");
        let e = Env {
            home: root.join("home"),
            cron: root.join("cron"),
            work: root.join("work"),
            bin: root.join("bin"),
            _root: root,
            url: url.to_string(),
        };
        for d in [&e.home.join("tole"), &e.cron, &e.work, &e.bin] {
            std::fs::create_dir_all(d).unwrap();
        }
        // The programs the job runs, and no `bwrap`.
        for p in ["sh", "touch"] {
            let src = ["/bin", "/usr/bin"]
                .iter()
                .map(|d| PathBuf::from(d).join(p))
                .find(|c| c.exists())
                .unwrap();
            std::os::unix::fs::symlink(src, e.bin.join(p)).unwrap();
        }
        e
    }

    fn user_config(&self, toml: &str) {
        std::fs::write(self.home.join("tole/config.toml"), toml).unwrap();
    }

    fn tole(&self, args: &[&str]) -> Output {
        let mut c = Command::new(env!("CARGO_BIN_EXE_tole"));
        c.args(args)
            .current_dir(&self.work)
            .env_clear()
            .env("TOLE_NO_UPDATE_CHECK", "1")
            .env("CODECORA_HOME", &self.home)
            .env("HOME", &self.home)
            .env("TOLE_CRON_DIR", &self.cron)
            .env("PATH", &self.bin)
            .env("TOLE_STREAM", "0")
            .env("TOLE_BASE_URL", &self.url)
            .env("TOLE_MODEL", "mock-model")
            .env("TOLE_API_KEY", "k")
            .env("TOLE_SKILL_APPROVAL", "off")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        c.spawn().unwrap().wait_with_output().unwrap()
    }

    fn ok(&self, args: &[&str]) -> String {
        let o = self.tole(args);
        assert!(o.status.success(), "{args:?}: {}", text(&o.stderr));
        text(&o.stdout)
    }

    fn create_run_job(&self) {
        let out = self.ok(&[
            "cron",
            "create",
            "@daily",
            "RUN the command",
            "--id",
            "j",
            "--allow",
            "run_command",
        ]);
        assert!(out.starts_with("created j (agent)"), "{out}");
    }

    /// What the model was told about its tool call (the `tool` message of the
    /// request after it).
    fn tool_reply(seen: &Seen) -> String {
        seen.lock()
            .unwrap()
            .iter()
            .flat_map(|b| b["messages"].as_array().cloned().unwrap_or_default())
            .filter(|m| m["role"] == "tool")
            .map(|m| m["content"].as_str().unwrap_or("").to_string())
            .collect::<Vec<_>>()
            .join("\n")
    }
}

/// The owner's `[sandbox.consumers] run_command = "required"` reaches a cron
/// agent run: with no `bwrap` the model's `run_command` is refused and the
/// command never runs. Without the fix the config was not loaded for `cron
/// run`, the consumer was off and the command ran unsandboxed.
#[test]
fn cron_run_applies_the_owner_sandbox_consumers() {
    let (url, seen) = spawn_mock();
    let e = Env::new(&url);
    e.user_config("[sandbox.consumers]\nrun_command = \"required\"\n");
    e.create_run_job();
    let _ = e.tole(&["cron", "run", "j"]);
    let told = Env::tool_reply(&seen);
    assert!(
        told.contains("sandbox required but unavailable") && told.contains("run_command"),
        "the model must be told the sandbox refused it: {told:?}"
    );
    assert!(
        !e.work.join("ran-marker").exists(),
        "the command must never run unsandboxed"
    );
}

/// Control: with the consumer off (no user config) the same job runs the
/// command, so the test above is about the config and nothing else.
#[test]
fn cron_run_without_a_sandbox_config_runs_the_command() {
    let (url, _seen) = spawn_mock();
    let e = Env::new(&url);
    e.create_run_job();
    let _ = e.tole(&["cron", "run", "j"]);
    assert!(e.work.join("ran-marker").exists(), "control: it ran");
}

/// A user config that does not parse is a failure for the commands that run
/// jobs (never "run with defaults"), while the commands that only edit files
/// keep working so the owner can repair it.
#[test]
fn a_broken_user_config_fails_cron_run_closed_but_not_cron_list() {
    let (url, seen) = spawn_mock();
    let e = Env::new(&url);
    e.create_run_job();
    e.user_config("this is [not toml\n");
    let o = e.tole(&["cron", "run", "j"]);
    assert!(!o.status.success(), "{}", text(&o.stdout));
    assert!(
        text(&o.stderr).contains("config"),
        "the error stays loud on stderr: {}",
        text(&o.stderr)
    );
    assert!(
        !e.work.join("ran-marker").exists() && seen.lock().unwrap().is_empty(),
        "nothing ran and the provider was never called"
    );
    let o = e.tole(&["cron", "tick"]);
    assert!(!o.status.success(), "tick fails closed too");
    assert!(
        text(&o.stderr).contains("config"),
        "and says why: {}",
        text(&o.stderr)
    );
    let o = e.tole(&["cron", "list"]);
    assert!(o.status.success(), "list still works: {}", text(&o.stderr));
}

/// REGRESSION GUARD (it passes with and without the #863 fix, on purpose): a
/// project config in the job's directory never configures cron, because it
/// would be a repository deciding how the host runs its own jobs. It keeps the
/// fix from loading the project layer by accident.
#[test]
fn regression_guard_a_project_config_is_never_read_for_cron() {
    let (url, _seen) = spawn_mock();
    let e = Env::new(&url);
    e.create_run_job();
    std::fs::create_dir_all(e.work.join(".tole")).unwrap();
    std::fs::write(
        e.work.join(".tole/config.toml"),
        "this is [not toml and would fail if it were read\n",
    )
    .unwrap();
    let _ = e.tole(&["cron", "run", "j"]);
    assert!(
        e.work.join("ran-marker").exists(),
        "the project file was not read (a parse error would have stopped the run)"
    );
}

/// An OS cron launches `tole cron tick` every minute and mails whatever it
/// prints to stderr (MAILTO): with a valid user config and no terminal a
/// successful `tick` or `run` must say nothing about the config. (A terminal
/// still gets the `using user config` line like `tole run`; the harness has no
/// pty, so that side is the line in `config::apply`.) That the config is still
/// loaded while quiet is `cron_run_applies_the_owner_sandbox_consumers`, which
/// also runs without a terminal.
#[test]
fn a_successful_tick_or_run_with_a_valid_user_config_is_silent_on_stderr() {
    let (url, _seen) = spawn_mock();
    let e = Env::new(&url);
    e.user_config("[sandbox.consumers]\nnetwork = \"none\"\n");
    let o = e.tole(&["cron", "tick"]);
    assert!(o.status.success(), "{}", text(&o.stderr));
    assert_eq!(text(&o.stderr), "", "tick: nothing for MAILTO to mail");
    e.create_run_job();
    let o = e.tole(&["cron", "run", "j"]);
    assert!(o.status.success(), "{}", text(&o.stderr));
    assert!(
        !text(&o.stderr).contains("using user config"),
        "run: {}",
        text(&o.stderr)
    );
}
