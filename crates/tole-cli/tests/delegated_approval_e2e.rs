//! #714 end to end with the REAL binary and a mock provider: a delegated
//! child (a `tole mission` started the way `delegate` starts one: the
//! delegation log and node in its environment) asks for a Write its allow
//! list does not cover. With `approvals = "owner"` it parks and the owner
//! answers with `tole delegation approvals`; deny and a timeout both deny;
//! with `off` (and unset) it is denied at once, as before.

#![cfg(feature = "shell-tools")]

use serde_json::{json, Value};
use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};
use tole_core::delegation::{
    Budget, BudgetRequest, DelegateRequest, Delegation, Goal, Mode, Spend,
};
use tole_core::ledger::Lease;

const FILE: &str = "approved.txt";

/// The child's model: its first step writes `approved.txt`; after a tool
/// result it completes the mission either way.
fn spawn_mock() -> String {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            std::thread::spawn(move || {
                let mut s = stream;
                let (mut buf, mut chunk) = (Vec::new(), [0u8; 16384]);
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
                let msgs = body["messages"].as_array().cloned().unwrap_or_default();
                let done = msgs.iter().any(|m| m["role"] == "tool");
                let msg = if done {
                    json!({"role": "assistant", "content": "ok\nMISSION_COMPLETE"})
                } else {
                    json!({"role": "assistant", "content": null, "tool_calls": [{
                        "id": "call_1", "type": "function",
                        "function": {"name": "write_file", "arguments":
                            json!({"path": FILE, "content": "approved by the owner\n"}).to_string()}}]})
                };
                let usage = json!({"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2});
                let (ctype, out) = if body["stream"] == true {
                    let mut o = String::new();
                    let delta = if done {
                        json!({"content": msg["content"]})
                    } else {
                        json!({"tool_calls": [{"index": 0, "id": "call_1", "function": {
                            "name": "write_file",
                            "arguments": msg["tool_calls"][0]["function"]["arguments"]}}]})
                    };
                    o.push_str(&format!(
                        "data: {}\n\n",
                        json!({"choices": [{"index": 0, "delta": delta, "finish_reason": null}]})
                    ));
                    o.push_str(&format!(
                        "data: {}\n\n",
                        json!({"choices": [], "usage": usage})
                    ));
                    o.push_str("data: [DONE]\n\n");
                    ("text/event-stream", o)
                } else {
                    let r = json!({"id": "c", "object": "chat.completion", "created": 1, "model": "mock",
                        "choices": [{"index": 0, "message": msg,
                            "finish_reason": if done { "stop" } else { "tool_calls" }}], "usage": usage});
                    ("application/json", r.to_string())
                };
                let _ = s.write_all(format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: {ctype}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{out}",
                    out.len()
                ).as_bytes());
            });
        }
    });
    format!("http://{addr}/v1")
}

struct World {
    home: PathBuf,
    root: PathBuf,
    base: String,
}

fn world(tag: &str, delegation_cfg: &str) -> World {
    let d = std::env::temp_dir().join(format!("tole-dap-e2e-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    let (home, root) = (d.join("home"), d.join("work"));
    std::fs::create_dir_all(home.join("tole")).unwrap();
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(home.join("tole/config.toml"), delegation_cfg).unwrap();
    World {
        home,
        root: root.canonicalize().unwrap(),
        base: spawn_mock(),
    }
}

impl World {
    /// The goal and one running child node, as `delegate` would record them.
    fn seed(&self) -> (PathBuf, String) {
        let data = self.home.join("tole");
        let log = tole_cli::delegate::log_path(&data, &self.root).expect("log path");
        std::fs::create_dir_all(log.parent().unwrap()).unwrap();
        let now = tole_cli::delegated_approver::real_now();
        let mut d = Delegation::open(tole_cli::ledger_file::FileStore::new(log.clone())).unwrap();
        let goal = Goal {
            id: "g",
            task: "t",
            budget: Budget {
                tokens: 1_000_000,
                steps: 100,
                deadline: now + 3600,
            },
            allow: &[],
            max_depth: 2,
            max_live: 8,
        };
        d.begin(&goal, now).unwrap();
        let r = DelegateRequest {
            profile: "coder",
            task: "write the file",
            mode: Mode::Execute,
            budget: BudgetRequest::default(),
            profile_allow: &[],
            workspace: None,
        };
        let node = d.delegate("g", &r, Spend::default(), now).unwrap().node_id;
        let lease = Lease {
            pid: std::process::id(),
            start_time: 1,
        };
        d.start(&node, lease, Some("sess-e2e"), now).unwrap();
        (log, node)
    }

    fn tole(&self) -> Command {
        let mut c = Command::new(env!("CARGO_BIN_EXE_tole"));
        c.current_dir(&self.root)
            .env("CODECORA_HOME", &self.home)
            .env("TOLE_NO_UPDATE_CHECK", "1")
            .env("TOLE_API_KEY", "test")
            .env("TOLE_MODEL", "mock")
            .env("TOLE_BASE_URL", &self.base)
            .env_remove("TOLE_UNDER_SESSION")
            .env_remove("TOLE_AGENT_DEPTH")
            .env_remove("TOLE_MEMORY")
            .env_remove("TOLE_TRUST")
            .stdin(Stdio::null());
        c
    }

    /// The delegated child: a mission with the delegation env, no `--allow`.
    fn child(&self, log: &Path, node: &str) -> Child {
        let sessions = self.root.join(".sessions");
        self.tole()
            .args(["mission", "-s"])
            .arg(sessions)
            .args(["--max-steps", "8", "--max-minutes", "2", "write the file"])
            .env("TOLE_AGENT_DEPTH", "1")
            .env("TOLE_DELEGATION_LOG", log)
            .env("TOLE_DELEGATION_NODE", node)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap()
    }

    fn owner(&self, args: &[&str]) -> Output {
        self.tole()
            .args(["delegation", "approvals"])
            .args(args)
            .output()
            .unwrap()
    }

    /// The id of the first pending request, waiting for it to appear.
    fn pending(&self) -> String {
        let end = Instant::now() + Duration::from_secs(60);
        while Instant::now() < end {
            let o = self.owner(&["list"]);
            let out = String::from_utf8_lossy(&o.stdout).to_string();
            if let Some(id) = out.split_whitespace().find(|w| w.starts_with("dap-")) {
                return id.to_string();
            }
            std::thread::sleep(Duration::from_millis(150));
        }
        panic!("no approval request appeared");
    }

    fn written(&self) -> Option<String> {
        std::fs::read_to_string(self.root.join(FILE)).ok()
    }

    fn audit(&self) -> String {
        let o = self.tole().args(["audit", "g"]).output().unwrap();
        String::from_utf8_lossy(&o.stdout).to_string()
    }
}

fn finish(c: Child) -> (String, i32) {
    let o = c.wait_with_output().unwrap();
    (
        format!(
            "{}{}",
            String::from_utf8_lossy(&o.stdout),
            String::from_utf8_lossy(&o.stderr)
        ),
        o.status.code().unwrap_or(-1),
    )
}

const OWNER: &str = "[delegation]\napprovals = \"owner\"\napproval_secs = 60\n";

#[test]
fn the_owner_allows_and_the_child_proceeds() {
    let w = world("allow", OWNER);
    let (log, node) = w.seed();
    let child = w.child(&log, &node);
    let id = w.pending();
    // The request is shown to the owner before anything is decided.
    let shown = w.owner(&["show", &id]);
    let text = String::from_utf8_lossy(&shown.stdout).to_string();
    assert!(text.contains("write_file") && text.contains(FILE), "{text}");
    assert!(w.written().is_none(), "nothing ran before the answer");
    let o = w.owner(&["allow", &id, "--yes"]);
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    let (out, code) = finish(child);
    assert_eq!(
        w.written().as_deref(),
        Some("approved by the owner\n"),
        "{out}"
    );
    assert_eq!(code, 0, "{out}");
    let audit = w.audit();
    assert!(
        audit.contains("decided allow by the owner (cli), consumed"),
        "{audit}"
    );
    // One call only: a second decision is refused.
    let again = w.owner(&["allow", &id, "--yes"]);
    assert!(!again.status.success());
}

#[test]
fn the_owner_denies_and_the_child_does_not_write() {
    let w = world("deny", OWNER);
    let (log, node) = w.seed();
    let child = w.child(&log, &node);
    let id = w.pending();
    assert!(w.owner(&["deny", &id]).status.success());
    let (out, _) = finish(child);
    assert!(w.written().is_none(), "{out}");
    assert!(
        w.audit().contains("decided deny by the owner (cli)"),
        "{}",
        w.audit()
    );
}

#[test]
fn no_answer_is_a_denial() {
    let w = world(
        "timeout",
        "[delegation]\napprovals = \"owner\"\napproval_secs = 2\napproval_level_secs = 1\n",
    );
    let (log, node) = w.seed();
    let started = Instant::now();
    let (out, _) = finish(w.child(&log, &node));
    assert!(
        started.elapsed() >= Duration::from_secs(2),
        "it waited for the window"
    );
    assert!(w.written().is_none(), "{out}");
    let audit = w.audit();
    assert!(
        audit.contains("expired (timeout), never decided"),
        "{audit}"
    );
}

#[test]
fn with_approvals_off_or_unset_the_child_is_denied_at_once_as_before() {
    for (tag, cfg) in [
        ("off", "[delegation]\napprovals = \"off\"\n"),
        ("unset", "[delegation]\nmax_depth = 2\n"),
    ] {
        let w = world(tag, cfg);
        let (log, node) = w.seed();
        let started = Instant::now();
        let (out, _) = finish(w.child(&log, &node));
        assert!(started.elapsed() < Duration::from_secs(30), "{tag}: {out}");
        assert!(w.written().is_none(), "{tag}: {out}");
        // No request was ever made: there is no approvals log.
        let dir = log.parent().unwrap();
        assert!(!dir.join("approvals.jsonl").exists(), "{tag}");
        let o = w.owner(&["list"]);
        assert!(
            String::from_utf8_lossy(&o.stdout).contains("no pending"),
            "{tag}"
        );
    }
}

#[test]
fn doctor_names_the_mode() {
    for (cfg, want) in [
        ("", "delegation approvals: off"),
        (
            "[delegation]\napprovals = \"owner\"\n",
            "delegation approvals: owner",
        ),
        (
            "[delegation]\napprovals = \"chain\"\n",
            "delegation approvals: chain",
        ),
    ] {
        let w = world("doctor", cfg);
        let o = w
            .tole()
            .args(["doctor", "--tier", "offline", "--no-ledger"])
            .output()
            .unwrap();
        let out = String::from_utf8_lossy(&o.stdout);
        assert!(out.contains(want), "{want} in\n{out}");
    }
}
