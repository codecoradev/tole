//! #740 per-session profiles over the REAL `tole serve` face: `profile` on
//! `POST /sessions` (create and resume) selects a `[profiles.<name>]` table of
//! the USER config for that session: persona, model, allow list and memory
//! room, recorded in the session log, fixed while the session lives.
//!
//! Hermetic: `CODECORA_HOME` and `HOME` are the test's, the provider is a
//! recording mock, and a fake `uteke` on `PATH` logs the rooms the memory
//! loop uses.

#![cfg(feature = "shell-tools")]

mod common;
use common::TestDir;
use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::path::PathBuf;
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

const CONFIG: &str = r#"
[profiles.kai]
system_prompt_file = "kai.md"
allow = ["write_file"]
[profiles.kai.models]
standard = "kai-model"

[profiles.ana]
system_prompt_file = "ana.md"
allow = ["read_file"]
memory_room = "room-ana"

[profiles.bare]
"#;

/// A serve daemon over a workspace, a home with a user config and a fake
/// `uteke` that logs its arguments.
struct Daemon {
    child: Child,
    port: u16,
    work: TestDir,
    home: TestDir,
    bin: TestDir,
    url: String,
    seen: Seen,
}

impl Daemon {
    fn spawn(config: &str, extra: &[&str]) -> Self {
        let (url, seen) = spawn_mock();
        let home = TestDir::new("tole-profile-serve-home");
        let work = TestDir::new("tole-profile-serve-work");
        let bin = TestDir::new("tole-profile-serve-bin");
        let dir = home.join("tole");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("config.toml"), config).unwrap();
        std::fs::write(dir.join("kai.md"), "You are KAI the repo operator.").unwrap();
        std::fs::write(dir.join("ana.md"), "You are ANA the researcher.").unwrap();
        for sub in ["a", "b"] {
            std::fs::create_dir_all(work.join(sub)).unwrap();
        }
        let log = bin.join("uteke.log");
        common::write_executable(
            bin.join("uteke"),
            format!("#!/bin/sh\necho \"$@\" >> {}\necho '[]'\n", log.display()),
        );
        let mut d = Self {
            child: dummy_child(),
            port: 0,
            work,
            home,
            bin,
            url,
            seen,
        };
        d.start(extra);
        d
    }

    fn start(&mut self, extra: &[&str]) {
        let (child, port) = common::spawn_listening(|port| {
            let mut cmd =
                common::hermetic_command(env!("CARGO_BIN_EXE_tole"), self.home.path(), &[]);
            // The test's own `uteke` shim stays first on PATH; the system
            // directories lose any real cora/uteke.
            let sys = common::path_without_sibling_binaries_from("/usr/bin:/bin".as_ref());
            cmd.args(["serve", "--port", &port.to_string(), "--token", "t0ken"])
                .args(["--workspace", self.work.to_str().unwrap()])
                .args(["--memory", "uteke"])
                .args(extra)
                .current_dir(self.work.path())
                .env("TOLE_NO_UPDATE_CHECK", "1")
                .env(
                    "PATH",
                    format!("{}:{}", self.bin.path().display(), sys.to_string_lossy()),
                )
                .env("TOLE_STREAM", "0")
                .env("TOLE_BASE_URL", &self.url)
                .env("TOLE_MODEL", "env-model")
                .env("TOLE_API_KEY", "sk-test")
                .stdin(Stdio::null())
                .stdout(Stdio::null());
            for k in [
                "TOLE_SYSTEM_PROMPT",
                "TOLE_MEMORY_NAMESPACE",
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

    /// Stop the process (its writer locks go with it) and start it again.
    fn restart(&mut self, extra: &[&str]) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        self.start(extra);
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

    fn post(&self, body: Value) -> (u16, Value) {
        self.http("POST", "/sessions", Some(&body.to_string()))
    }

    fn cwd(&self, sub: &str) -> String {
        self.work.join(sub).to_str().unwrap().to_string()
    }

    fn create(&self, body: Value) -> String {
        let (code, b) = self.post(body);
        assert_eq!(code, 200, "{b}");
        b["sessionId"].as_str().unwrap().to_string()
    }

    fn prompt(&self, id: &str, text: &str) -> Value {
        let (code, b) = self.http(
            "POST",
            &format!("/sessions/{id}/prompt"),
            Some(&json!({"text": text}).to_string()),
        );
        assert_eq!(code, 200, "{b}");
        b
    }

    fn file(&self, sub: &str, id: &str) -> PathBuf {
        self.work
            .join(sub)
            .join(".tole/sessions")
            .join(format!("{id}.jsonl"))
    }

    /// The last recorded `fact/profile` name of a session file.
    fn recorded(&self, sub: &str, id: &str) -> Option<String> {
        let s = tole_core::storage::JsonlStorage::open_read_only(self.file(sub, id)).unwrap();
        s.get_register("fact", "profile")
            .map(|v| v["name"].as_str().unwrap().to_string())
    }

    /// The most recent provider request.
    fn last_request(&self) -> Value {
        self.seen.lock().unwrap().last().cloned().unwrap()
    }

    fn memory_log(&self) -> String {
        std::fs::read_to_string(self.bin.join("uteke.log")).unwrap_or_default()
    }

    fn session_files(&self, sub: &str) -> usize {
        std::fs::read_dir(self.work.join(sub).join(".tole/sessions"))
            .map(|d| d.flatten().count())
            .unwrap_or(0)
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn dummy_child() -> Child {
    Command::new("true").spawn().unwrap()
}

fn system_text(req: &Value) -> String {
    req["messages"][0]["content"]
        .as_str()
        .unwrap_or_default()
        .to_string()
}

/// Create with a profile: persona, model and memory room are the profile's,
/// the choice is recorded, and the session list shows it.
#[test]
fn a_session_created_with_a_profile_uses_its_persona_model_and_room() {
    let d = Daemon::spawn(CONFIG, &[]);
    let (code, body) = d.post(json!({"cwd": d.cwd("a"), "profile": "kai"}));
    assert_eq!(code, 200, "{body}");
    assert_eq!(body["profile"], "kai");
    let id = body["sessionId"].as_str().unwrap().to_string();
    d.prompt(&id, "hello");
    let req = d.last_request();
    assert_eq!(req["model"], "kai-model", "{req}");
    assert_eq!(system_text(&req), "You are KAI the repo operator.");
    assert_eq!(d.recorded("a", &id).as_deref(), Some("kai"));
    // The memory loop recalled from and remembered into the profile's room.
    let log = d.memory_log();
    assert!(log.contains("room recall --limit"), "{log}");
    assert!(log.contains("profile-kai"), "{log}");
    assert!(!log.contains("repo-"), "no default room was used: {log}");
    // The live list shows each session's profile.
    let (_, list) = d.http("GET", "/sessions", None);
    assert_eq!(list["sessions"][0]["profile"], "kai", "{list}");
}

/// Two sessions, two profiles, in parallel: nothing leaks between them.
#[test]
fn two_profiles_in_one_daemon_do_not_leak_into_each_other() {
    let d = Daemon::spawn(CONFIG, &[]);
    let kai = d.create(json!({"cwd": d.cwd("a"), "profile": "kai"}));
    let ana = d.create(json!({"cwd": d.cwd("b"), "profile": "ana"}));
    d.prompt(&kai, "one");
    let k = d.last_request();
    d.prompt(&ana, "two");
    let a = d.last_request();
    assert_eq!(system_text(&k), "You are KAI the repo operator.");
    assert_eq!(system_text(&a), "You are ANA the researcher.");
    assert_eq!(k["model"], "kai-model");
    // ana sets no model: the server default.
    assert_eq!(a["model"], "env-model");
    let log = d.memory_log();
    assert!(
        log.contains("profile-kai") && log.contains("room-ana"),
        "{log}"
    );
    assert_eq!(d.recorded("a", &kai).as_deref(), Some("kai"));
    assert_eq!(d.recorded("b", &ana).as_deref(), Some("ana"));
}

/// Without `profile` nothing changes: no key in the answer, no register, the
/// server model, no persona, the default room.
#[test]
fn without_a_profile_the_session_is_unchanged() {
    let d = Daemon::spawn(CONFIG, &[]);
    let (code, body) = d.post(json!({"cwd": d.cwd("a")}));
    assert_eq!(code, 200, "{body}");
    assert!(body.get("profile").is_none(), "{body}");
    let id = body["sessionId"].as_str().unwrap();
    d.prompt(id, "hello");
    let req = d.last_request();
    assert_eq!(req["model"], "env-model");
    assert!(!system_text(&req).contains("KAI"), "{req}");
    assert_eq!(d.recorded("a", id), None);
    assert!(d.memory_log().contains("repo-"), "{}", d.memory_log());
    // `"profile": null` is the same as absent.
    let (code, body) = d.post(json!({"cwd": d.cwd("a"), "profile": null}));
    assert_eq!(code, 200, "{body}");
    assert!(body.get("profile").is_none());
    // The list has no `profile` key for a session without one.
    let (_, list) = d.http("GET", "/sessions", None);
    for entry in list["sessions"].as_array().unwrap() {
        assert!(entry.get("profile").is_none(), "{list}");
        assert_eq!(entry.as_object().unwrap().len(), 2, "{list}");
    }
}

/// An unknown profile is a clear 4xx, never a silent default, and creates no
/// session file; a hostile or non-string value is refused before any lookup.
#[test]
fn an_unknown_or_malformed_profile_is_a_4xx_and_creates_nothing() {
    let d = Daemon::spawn(CONFIG, &[]);
    let (code, body) = d.post(json!({"cwd": d.cwd("a"), "profile": "ghost"}));
    assert_eq!(code, 400, "{body}");
    let msg = body["error"].as_str().unwrap();
    assert!(msg.contains("unknown profile `ghost`"), "{msg}");
    for bad in ["../etc", "KAI", "a b", "x\ny", "-kai", ""] {
        let (code, body) = d.post(json!({"cwd": d.cwd("a"), "profile": bad}));
        assert_eq!(code, 400, "{bad:?}: {body}");
        assert!(
            !body["error"].as_str().unwrap().contains(bad) || bad.is_empty(),
            "echoed {bad:?}"
        );
    }
    for bad in [json!(7), json!(["kai"]), json!({"name": "kai"})] {
        let (code, body) = d.post(json!({"cwd": d.cwd("a"), "profile": bad}));
        assert_eq!(code, 400, "{body}");
    }
    assert_eq!(d.session_files("a"), 0, "a refused request creates no file");
    // Needs the token like every route.
    let mut s = std::net::TcpStream::connect(("127.0.0.1", d.port)).unwrap();
    let b = r#"{"profile":"kai"}"#;
    write!(
        s,
        "POST /sessions HTTP/1.1\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{b}",
        b.len()
    )
    .unwrap();
    let mut first = String::new();
    BufReader::new(s).read_line(&mut first).unwrap();
    assert!(first.contains("401"), "{first}");
}

/// With no user config at all, every profile is unknown.
#[test]
fn no_user_config_means_no_selectable_profile() {
    let d = Daemon::spawn("", &[]);
    let (code, body) = d.post(json!({"cwd": d.cwd("a"), "profile": "kai"}));
    assert_eq!(code, 400, "{body}");
}

/// A request cannot name the memory room: refused on create and on resume.
#[test]
fn a_request_cannot_set_the_memory_room() {
    let d = Daemon::spawn(CONFIG, &[]);
    for body in [
        json!({"cwd": d.cwd("a"), "memory_room": "stolen"}),
        json!({"cwd": d.cwd("a"), "profile": "kai", "memory_room": "stolen"}),
        json!({"cwd": d.cwd("a"), "profile": "kai", "memory_room": null}),
        json!({"cwd": d.cwd("a"), "resume": "srv-x", "memory_room": "stolen"}),
    ] {
        let (code, b) = d.post(body);
        assert_eq!(code, 400, "{b}");
        assert!(b["error"].as_str().unwrap().contains("memory_room"), "{b}");
    }
    assert_eq!(d.session_files("a"), 0);
    // The profile's own room is what the session uses.
    let id = d.create(json!({"cwd": d.cwd("a"), "profile": "kai"}));
    d.prompt(&id, "hi");
    assert!(!d.memory_log().contains("stolen"));
}

/// Resume: the request's profile wins over the recorded one and is recorded
/// as a new entry; without one the recorded profile is used again; a live
/// session's profile is fixed.
#[test]
fn resume_reconciles_request_over_recorded_and_fixes_a_live_profile() {
    let mut d = Daemon::spawn(CONFIG, &[]);
    let id = d.create(json!({"cwd": d.cwd("a"), "profile": "kai"}));
    d.prompt(&id, "first");
    d.restart(&[]);

    // The request's profile wins and is appended to the log.
    let (code, b) = d.post(json!({"cwd": d.cwd("a"), "resume": id, "profile": "ana"}));
    assert_eq!(code, 200, "{b}");
    assert_eq!(b["profile"], "ana");
    assert_eq!(d.recorded("a", &id).as_deref(), Some("ana"));
    d.prompt(&id, "second");
    let req = d.last_request();
    assert_eq!(system_text(&req), "You are ANA the researcher.");
    assert_eq!(req["model"], "env-model");
    // Append-only: the earlier turn is still in the log.
    let s = tole_core::storage::JsonlStorage::open_read_only(d.file("a", &id)).unwrap();
    assert!(s.entries().len() >= 4);

    // Live: another profile is refused (fixed for the process), the same one
    // and none are fine.
    let (code, b) = d.post(json!({"cwd": d.cwd("a"), "resume": id, "profile": "kai"}));
    assert_eq!(code, 409, "{b}");
    assert_eq!(d.recorded("a", &id).as_deref(), Some("ana"));
    let (code, b) = d.post(json!({"cwd": d.cwd("a"), "resume": id, "profile": "ana"}));
    assert_eq!((code, &b["alreadyLive"]), (200, &json!(true)), "{b}");
    let (code, _) = d.post(json!({"cwd": d.cwd("a"), "resume": id}));
    assert_eq!(code, 200);

    // A restart and a resume WITHOUT a profile keeps the recorded one.
    d.restart(&[]);
    let (code, b) = d.post(json!({"cwd": d.cwd("a"), "resume": id}));
    assert_eq!(code, 200, "{b}");
    assert_eq!(b["profile"], "ana");
    d.prompt(&id, "third");
    assert_eq!(
        system_text(&d.last_request()),
        "You are ANA the researcher."
    );
    // Resuming an unknown profile is refused and leaves the log alone.
    d.restart(&[]);
    let (code, _) = d.post(json!({"cwd": d.cwd("a"), "resume": id, "profile": "ghost"}));
    assert_eq!(code, 400);
}

/// A session recorded under a profile that no longer exists is not resumed
/// into the daemon's defaults: 409, and an explicit profile still works.
#[test]
fn resume_of_a_session_whose_profile_was_removed_is_refused() {
    let mut d = Daemon::spawn(CONFIG, &[]);
    let id = d.create(json!({"cwd": d.cwd("a"), "profile": "kai"}));
    d.prompt(&id, "first");
    let cfg = d.home.join("tole/config.toml");
    std::fs::write(&cfg, "[profiles.ana]\nsystem_prompt_file = \"ana.md\"\n").unwrap();
    d.restart(&[]);
    let (code, b) = d.post(json!({"cwd": d.cwd("a"), "resume": id}));
    assert_eq!(code, 409, "{b}");
    let msg = b["error"].as_str().unwrap();
    assert!(msg.contains("`kai`") && msg.contains("explicit"), "{msg}");
    let (code, b) = d.post(json!({"cwd": d.cwd("a"), "resume": id, "profile": "ana"}));
    assert_eq!(code, 200, "{b}");
}

/// Two resumes of one idle session with different profiles: exactly one
/// opens it, the other is refused, and the log holds the winner.
#[test]
fn concurrent_resumes_with_different_profiles_have_one_winner() {
    let mut d = Daemon::spawn(CONFIG, &[]);
    let id = d.create(json!({"cwd": d.cwd("a")}));
    d.prompt(&id, "first");
    d.restart(&[]);
    let d = Arc::new(d);
    let results: Vec<(u16, Value)> = ["kai", "ana"]
        .into_iter()
        .map(|p| {
            let d = Arc::clone(&d);
            let (cwd, id) = (d.cwd("a"), id.clone());
            std::thread::spawn(move || d.post(json!({"cwd": cwd, "resume": id, "profile": p})))
        })
        .collect::<Vec<_>>()
        .into_iter()
        .map(|h| h.join().unwrap())
        .collect();
    let ok: Vec<&(u16, Value)> = results.iter().filter(|(c, _)| *c == 200).collect();
    assert_eq!(ok.len(), 1, "{results:?}");
    assert!(results.iter().any(|(c, _)| *c == 409), "{results:?}");
    let winner = ok[0].1["profile"].as_str().unwrap();
    assert_eq!(d.recorded("a", &id).as_deref(), Some(winner));
}

/// The profile's allow list decides what runs without an approval: `kai`
/// pre-authorizes `write_file`, `ana` (allow = read_file) does not, so her
/// write queues for a remote decision and writes nothing.
#[test]
fn a_profile_allow_list_restricts_what_a_session_may_do_unasked() {
    let d = Daemon::spawn(CONFIG, &[]);
    let kai = d.create(json!({"cwd": d.cwd("a"), "profile": "kai"}));
    d.prompt(&kai, "TOOL_MISSION");
    assert!(d.work.join("a/approved.txt").is_file(), "kai may write");
    let ana = d.create(json!({"cwd": d.cwd("b"), "profile": "ana"}));
    d.prompt(&ana, "TOOL_MISSION");
    assert!(!d.work.join("b/approved.txt").exists(), "ana may not");
    let (_, q) = d.http("GET", &format!("/approvals?session={ana}"), None);
    let pending = q["approvals"].as_array().unwrap();
    assert_eq!(pending.len(), 1, "{q}");
    assert_eq!(pending[0]["tool"], "write_file", "{q}");
    // A profile-less session keeps the daemon's list (none): it queues too.
    let plain = d.create(json!({"cwd": d.cwd("b")}));
    d.prompt(&plain, "TOOL_MISSION");
    assert!(!d.work.join("b/approved.txt").exists());
}

/// A profile never widens what `--allow` of the serve command line permits:
/// the flag pins the daemon's list and the profile's is ignored.
#[test]
fn the_allow_flag_pins_the_list_and_a_profile_cannot_widen_it() {
    let d = Daemon::spawn(CONFIG, &["--allow", "read_file"]);
    let kai = d.create(json!({"cwd": d.cwd("a"), "profile": "kai"}));
    d.prompt(&kai, "TOOL_MISSION");
    assert!(
        !d.work.join("a/approved.txt").exists(),
        "kai's allow = write_file must not widen `--allow read_file`"
    );
    let (_, q) = d.http("GET", &format!("/approvals?session={kai}"), None);
    assert_eq!(q["approvals"].as_array().unwrap().len(), 1, "{q}");
}

/// A profile that sets nothing leaves the daemon's allow list and server
/// model, and still gets its own memory room.
#[test]
fn an_empty_profile_inherits_the_daemon_defaults() {
    let d = Daemon::spawn(CONFIG, &["--allow", "write_file"]);
    let id = d.create(json!({"cwd": d.cwd("a"), "profile": "bare"}));
    d.prompt(&id, "TOOL_MISSION");
    assert!(d.work.join("a/approved.txt").is_file());
    assert_eq!(d.last_request()["model"], "env-model");
    assert!(
        d.memory_log().contains("profile-bare"),
        "{}",
        d.memory_log()
    );
}

/// Only the USER config defines selectable profiles: a project config in the
/// daemon's directory cannot add one (it is ignored at start-up).
#[test]
fn a_project_config_cannot_add_a_selectable_profile() {
    let mut d = Daemon::spawn(CONFIG, &[]);
    let proj = d.work.join(".tole");
    std::fs::create_dir_all(&proj).unwrap();
    std::fs::write(
        proj.join("config.toml"),
        "[profiles.evil]\nallow = [\"*\"]\n",
    )
    .unwrap();
    // An untrusted project config stops the daemon at start-up (fail
    // closed); trusted, its `profiles` are still ignored.
    let trust = common::hermetic_command(env!("CARGO_BIN_EXE_tole"), d.home.path(), &[])
        .args(["config", "trust", "--yes"])
        .current_dir(d.work.path())
        .output()
        .unwrap();
    assert!(trust.status.success(), "{trust:?}");
    d.restart(&[]);
    let (code, b) = d.post(json!({"cwd": d.cwd("a"), "profile": "evil"}));
    assert_eq!(code, 400, "{b}");
    // The user's own profile still works.
    let (code, _) = d.post(json!({"cwd": d.cwd("a"), "profile": "kai"}));
    assert_eq!(code, 200);
}

/// A log whose recorded profile name is malformed, or that cannot be read to
/// look for one, is never resumed under the daemon's defaults (which can be
/// wider than the recorded profile): 409 / 500 asking for an explicit profile.
#[test]
fn a_tampered_or_unreadable_log_is_not_resumed_into_the_defaults() {
    let mut d = Daemon::spawn(CONFIG, &[]);
    let id = d.create(json!({"cwd": d.cwd("a"), "profile": "kai"}));
    d.prompt(&id, "first");
    d.restart(&[]);
    // A tampered entry: a recorded name that is not a profile name.
    {
        let mut s = tole_core::storage::JsonlStorage::open(d.file("a", &id)).unwrap();
        s.commit(tole_core::storage::Commit::new().register(
            tole_core::register::RegisterWrite::set(
                "fact",
                "profile",
                json!({"name": "../../etc/passwd", "atMs": 1}),
            ),
        ))
        .unwrap();
    }
    let (code, b) = d.post(json!({"cwd": d.cwd("a"), "resume": id}));
    assert_eq!(code, 409, "{b}");
    let msg = b["error"].as_str().unwrap();
    assert!(msg.contains("explicit") && !msg.contains("passwd"), "{msg}");
    // An explicit profile still resumes it and is recorded over the bad one.
    let (code, b) = d.post(json!({"cwd": d.cwd("a"), "resume": id, "profile": "ana"}));
    assert_eq!(code, 200, "{b}");
    assert_eq!(d.recorded("a", &id).as_deref(), Some("ana"));

    // A log that cannot be read at all.
    let broken = d.create(json!({"cwd": d.cwd("b")}));
    d.restart(&[]);
    std::fs::write(d.file("b", &broken), "not a session log\n").unwrap();
    let (code, b) = d.post(json!({"cwd": d.cwd("b"), "resume": broken}));
    assert_eq!(code, 500, "{b}");
    assert!(b["error"].as_str().unwrap().contains("explicit"), "{b}");
}
