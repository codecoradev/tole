//! #579 (closing part) end-to-end with the REAL binary: `tole cron create
//! --skill/--profile`, the profile applied to an agent run (persona, model,
//! skills, memory room, allow ∩ job allow), `blocked` when a skill or the
//! profile is gone, and `cron.max_concurrent`.
//!
//! Hermetic: `CODECORA_HOME`, `TOLE_CRON_DIR` and the provider variables
//! are set on the child only; the provider is a local mock that records
//! request bodies; `uteke` on PATH is a fake that logs its argv.
#![cfg(all(unix, feature = "shell-tools"))]

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
    let d = std::env::temp_dir().join(format!("tole-cron-prof-{}-{n}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d.canonicalize().unwrap()
}

fn text(b: &[u8]) -> String {
    String::from_utf8_lossy(b).to_string()
}

type Seen = Arc<Mutex<Vec<Value>>>;

fn completion(message: Value) -> Value {
    json!({"id": "c", "object": "chat.completion", "created": 1, "model": "mock",
        "choices": [{"index": 0, "message": message,
            "finish_reason": if message["tool_calls"].is_array() { "tool_calls" } else { "stop" }}],
        "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2}})
}

/// A prompt with WRITE asks for a `write_file` call first; anything else
/// gets a final answer.
fn reply(body: &Value) -> Value {
    let msgs = body["messages"].as_array().cloned().unwrap_or_default();
    let wants_write = msgs
        .iter()
        .any(|m| m["role"] == "user" && m["content"].as_str().unwrap_or("").contains("WRITE"));
    let after_tool = msgs.last().is_some_and(|m| m["role"] == "tool");
    if wants_write && !after_tool {
        completion(
            json!({"role": "assistant", "content": null, "tool_calls": [{"id": "call_1",
            "type": "function", "function": {"name": "write_file",
            "arguments": json!({"path": "out.txt", "content": "x"}).to_string()}}]}),
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
}

impl Env {
    fn new(url: &str) -> Self {
        let root = tmpdir();
        let e = Env {
            home: root.join("home"),
            cron: root.join("cron"),
            work: root.join("work"),
            bin: root.join("bin"),
            url: url.to_string(),
        };
        for d in [&e.home, &e.cron, &e.work, &e.bin] {
            std::fs::create_dir_all(d).unwrap();
        }
        std::fs::create_dir_all(e.home.join("tole")).unwrap();
        e
    }

    fn config(&self, toml: &str) {
        std::fs::write(self.home.join("tole").join("config.toml"), toml).unwrap();
    }

    fn skill(&self, name: &str, body: &str) {
        let d = self.work.join("skills").join(name);
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(
            d.join("SKILL.md"),
            format!("---\nname: {name}\ndescription: \"d\"\n---\n{body}"),
        )
        .unwrap();
    }

    fn tole(&self, args: &[&str]) -> Output {
        let mut c = Command::new(env!("CARGO_BIN_EXE_tole"));
        c.args(args)
            .current_dir(&self.work)
            .env("TOLE_NO_UPDATE_CHECK", "1")
            .env("CODECORA_HOME", &self.home)
            .env("HOME", &self.home)
            .env("TOLE_CRON_DIR", &self.cron)
            .env("PATH", format!("{}:/usr/bin:/bin", self.bin.display()))
            .env("TOLE_STREAM", "0")
            .env("TOLE_BASE_URL", &self.url)
            .env("TOLE_MODEL", "env-model")
            .env("TOLE_API_KEY", "k")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
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
        if self.bin.join("uteke").exists() {
            c.env("TOLE_MEMORY", "uteke");
        }
        let child = c.spawn().unwrap();
        child.wait_with_output().unwrap()
    }

    fn ok(&self, args: &[&str]) -> String {
        let o = self.tole(args);
        assert!(o.status.success(), "{args:?}: {}", text(&o.stderr));
        text(&o.stdout)
    }
}

const PROFILE: &str = r#"
[profiles.ops]
system_prompt_file = "ops.md"
allow = ["write_file"]
grants = ["GH_TOKEN"]
[profiles.ops.models]
standard = "ops-model"

[profiles.narrow]
allow = ["edit_file"]
"#;

#[test]
fn a_profiled_job_runs_with_persona_model_skills_and_memory_room() {
    let (url, seen) = spawn_mock();
    let e = Env::new(&url);
    e.config(PROFILE);
    std::fs::write(
        e.home.join("tole").join("ops.md"),
        "You are the ops persona.",
    )
    .unwrap();
    e.skill("lint", "run the linters");
    // A fake uteke: logs its argv, recalls nothing.
    let log = e.bin.join("uteke.log");
    let fake = e.bin.join("uteke");
    std::fs::write(
        &fake,
        format!("#!/bin/sh\necho \"$@\" >> {}\necho '[]'\n", log.display()),
    )
    .unwrap();
    std::fs::set_permissions(&fake, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();

    let out = e.ok(&[
        "cron",
        "create",
        "@daily",
        "say HI",
        "--id",
        "p",
        "--skill",
        "lint",
        "--profile",
        "ops",
    ]);
    assert!(out.starts_with("created p (agent)"), "{out}");
    let ran = e.ok(&["cron", "run", "p"]);
    assert!(ran.contains(" ok"), "{ran}");
    let body = seen.lock().unwrap()[0].clone();
    assert_eq!(body["model"], "ops-model", "{body}");
    assert_eq!(
        body["messages"][0],
        json!({"role": "system",
               "content": "You are the ops persona.\n\n# Skill: lint\nrun the linters"})
    );
    let argv = std::fs::read_to_string(&log).unwrap_or_default();
    assert!(argv.contains("profile-ops"), "memory room: {argv}");

    // The job's --model pin beats the profile's model.
    e.ok(&[
        "cron",
        "create",
        "@daily",
        "say HI",
        "--id",
        "pin",
        "--profile",
        "ops",
        "--model",
        "pin-model",
    ]);
    e.ok(&["cron", "run", "pin"]);
    let last = seen.lock().unwrap().last().unwrap().clone();
    assert_eq!(last["model"], "pin-model");
}

#[test]
fn writes_need_both_the_job_and_the_profile_allow_list() {
    let (url, seen) = spawn_mock();
    let e = Env::new(&url);
    e.config(PROFILE);
    std::fs::write(e.home.join("tole").join("ops.md"), "ops").unwrap();
    // Job and profile both allow write_file: the Write runs unattended.
    e.ok(&[
        "cron",
        "create",
        "@daily",
        "WRITE it",
        "--id",
        "both",
        "--allow",
        "write_file",
        "--profile",
        "ops",
    ]);
    assert!(e.ok(&["cron", "run", "both"]).contains(" ok"));
    assert!(e.work.join("out.txt").exists());
    std::fs::remove_file(e.work.join("out.txt")).unwrap();
    // The job allows it, the profile does not: refused, never wider.
    e.ok(&[
        "cron",
        "create",
        "@daily",
        "WRITE it",
        "--id",
        "jobonly",
        "--allow",
        "write_file",
        "--profile",
        "narrow",
    ]);
    let ran = e.ok(&["cron", "run", "jobonly"]);
    assert!(ran.contains("awaitingapproval"), "{ran}");
    assert!(!e.work.join("out.txt").exists());
    // The profile allows it, the job does not: refused too.
    e.ok(&[
        "cron",
        "create",
        "@daily",
        "WRITE it",
        "--id",
        "profonly",
        "--profile",
        "ops",
    ]);
    let ran = e.ok(&["cron", "run", "profonly"]);
    assert!(ran.contains("awaitingapproval"), "{ran}");
    assert!(!e.work.join("out.txt").exists());
    assert!(!seen.lock().unwrap().is_empty());
}

#[test]
fn bad_skills_and_profiles_are_refused_on_save_and_block_the_run_later() {
    let (url, seen) = spawn_mock();
    let e = Env::new(&url);
    e.config(PROFILE);
    std::fs::write(e.home.join("tole").join("ops.md"), "ops").unwrap();
    e.skill("lint", "b");
    for (args, needle) in [
        (vec!["--skill", "absent"], "--skill"),
        (vec!["--profile", "nobody"], "unknown profile"),
    ] {
        let mut a = vec!["cron", "create", "@daily", "p"];
        a.extend(args);
        let o = e.tole(&a);
        assert!(!o.status.success());
        assert!(text(&o.stderr).contains(needle), "{}", text(&o.stderr));
    }
    let o = e.tole(&[
        "cron", "create", "@daily", "--script", "echo x", "--skill", "lint",
    ]);
    assert!(!o.status.success());
    assert!(
        text(&o.stderr).contains("agent jobs only"),
        "{}",
        text(&o.stderr)
    );

    e.ok(&[
        "cron", "create", "@daily", "p", "--id", "s", "--skill", "lint",
    ]);
    e.ok(&["cron", "create", "@daily", "p", "--id", "q"]);
    // edit is validated like create; only create and edit take the flags.
    let o = e.tole(&["cron", "edit", "q", "--profile", "nobody"]);
    assert!(!o.status.success(), "{}", text(&o.stdout));
    e.ok(&["cron", "edit", "q", "--profile", "ops"]);
    let o = e.tole(&["cron", "list", "--profile", "ops"]);
    assert!(
        text(&o.stderr).contains("`cron create` and `cron edit` only"),
        "{}",
        text(&o.stderr)
    );
    std::fs::remove_dir_all(e.work.join("skills/lint")).unwrap();
    e.config("");
    let ran = e.ok(&["cron", "run", "s"]);
    assert!(ran.contains("blocked 1"), "{ran}");
    let ran = e.ok(&["cron", "run", "q"]);
    assert!(ran.contains("blocked 1"), "{ran}");
    assert!(
        seen.lock().unwrap().is_empty(),
        "a blocked run spends nothing"
    );
}

#[test]
fn max_concurrent_comes_from_the_user_config() {
    let (url, _) = spawn_mock();
    let e = Env::new(&url);
    assert!(e
        .ok(&["cron", "status"])
        .contains("runs per tick: at most 4"));
    e.config("[cron]\nmax_concurrent = 9\n");
    assert!(e
        .ok(&["cron", "status"])
        .contains("runs per tick: at most 9"));
    e.config("[cron]\nmax_concurrent = 0\n");
    let o = e.tole(&["cron", "status"]);
    assert!(
        text(&o.stderr).contains("cron.max_concurrent must be 1 to 64"),
        "{}",
        text(&o.stderr)
    );
}
