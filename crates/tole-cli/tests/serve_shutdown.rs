//! #1014: graceful shutdown of the REAL `tole serve` on SIGTERM and SIGINT.
//!
//! What an operator (a container's PID 1, a systemd unit, a desktop supervisor)
//! relies on: the signal is handled, a running turn is ended through the existing
//! detach path and stays resumable, a webhook delivery in flight is finished and
//! not abandoned, the process exits 0 once nothing is left and within the grace
//! period, and a local socket's file and lock are released. A second signal, or a
//! turn that outlives the grace period, ends the wait with a non-zero code.
//!
//! Everything waits on a fact (the provider mock's gate, the 503 a draining serve
//! answers, the child's exit), never on a sleep of a guessed length.

// Spawns the real `tole serve` binary (`shell-tools` feature, #349); signals are
// a Unix feature.
#![cfg(all(feature = "shell-tools", unix))]
#![allow(unsafe_code)] // SAFETY: test-only libc call on a path or descriptor the test owns.

use serde_json::{json, Value};
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::os::fd::AsRawFd;
use std::path::PathBuf;
use std::process::{Child, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use tole_cli::cron::{self, store, CreateSpec};
use tole_core::ledger::DeliveryStatus;
use tole_core::webhook;

mod common;
mod serve_harness;
use common::{wait_exit, wait_for, wait_until, Gate, TestDir, WAIT};
use serve_harness::{home_of, new_session, prompt, workspace_dir, Serve};

/// The default grace period of `tole serve`; a clean drain must beat it.
const GRACE: Duration = Duration::from_secs(20);
const HOOK_KEY: &str = "hook-key-for-serve-shutdown-0123456789";
const SIGN_KEY: &str = "destination-signing-key-shutdown-9f8e7d6c";
const SIGN_VAR: &str = "PUSHSIG_SHUTDOWN_E2E";

fn signal(child: &Child, sig: i32) {
    // SAFETY: plain `kill(2)` to a child this test spawned and has not reaped.
    assert_eq!(unsafe { libc::kill(child.id() as i32, sig) }, 0);
}

// ----------------------------------------------------------------- HTTP

/// One request that tolerates the connection ending without an answer (the
/// server was stopped): `(status, body)` or `None`.
fn raw_http(port: u16, method: &str, path: &str, body: &str) -> Option<(u16, Value)> {
    let mut s = TcpStream::connect(("127.0.0.1", port)).ok()?;
    s.set_read_timeout(Some(Duration::from_secs(60))).ok()?;
    let req = format!(
        "{method} {path} HTTP/1.1\r\nHost: x\r\nAuthorization: Bearer t0ken\r\n\
         Content-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    s.write_all(req.as_bytes()).ok()?;
    let mut out = String::new();
    let _ = s.read_to_string(&mut out);
    let code = out.split_whitespace().nth(1)?.parse().ok()?;
    let payload = out.split("\r\n\r\n").nth(1).unwrap_or("");
    Some((
        code,
        serde_json::from_str(payload.trim()).unwrap_or(Value::Null),
    ))
}

/// Waits until the server answers 503: it has seen the signal, asked every
/// running turn to detach and stopped the helper threads (the sweep and the stop
/// flag come before the first refusal).
fn wait_draining(port: u16) {
    wait_until(WAIT, "the server to start refusing requests (503)", || {
        raw_http(port, "GET", "/health", "").is_some_and(|(code, _)| code == 503)
    });
}

// ---------------------------------------------------------- provider mock

/// Reads one HTTP request and returns its JSON body.
fn read_json_body(s: &mut TcpStream) -> Value {
    let _ = s.set_read_timeout(Some(Duration::from_secs(10)));
    let mut buf = Vec::new();
    let mut chunk = [0u8; 16384];
    loop {
        let n = s.read(&mut chunk).unwrap_or(0);
        buf.extend_from_slice(&chunk[..n]);
        if let Some(h) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            let head = String::from_utf8_lossy(&buf[..h]).to_ascii_lowercase();
            let len = head
                .lines()
                .find_map(|l| l.strip_prefix("content-length:")?.trim().parse().ok())
                .unwrap_or(0usize);
            if buf.len() >= h + 4 + len {
                return serde_json::from_slice(&buf[h + 4..h + 4 + len]).unwrap_or(Value::Null);
            }
        }
        if n == 0 {
            return Value::Null;
        }
    }
}

/// A chat-completions mock. A turn whose last user message says `HOLD` is held
/// at `gate` when its first request arrives, then answers with a read-only tool
/// call; once a tool result follows (and for any other prompt) it answers
/// `SHUTDOWN-ANSWER`. Streamed and plain requests are both served.
fn spawn_gated_mock(gate: Arc<Gate>) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let gate = Arc::clone(&gate);
            std::thread::spawn(move || {
                let mut s = stream;
                let body = read_json_body(&mut s);
                let msgs = body["messages"].as_array().cloned().unwrap_or_default();
                let last_user = msgs.iter().rposition(|m| m["role"] == "user");
                let has_tool_result =
                    last_user.is_some_and(|i| msgs[i + 1..].iter().any(|m| m["role"] == "tool"));
                let hold = last_user
                    .and_then(|i| msgs[i]["content"].as_str())
                    .is_some_and(|t| t.contains("HOLD"));
                let wants_tool = hold && !has_tool_result;
                if wants_tool {
                    gate.hold();
                }
                let streamed = body.get("stream").and_then(Value::as_bool) == Some(true);
                let args = r#"{"path":"note.txt"}"#;
                let resp = if streamed {
                    let delta = if wants_tool {
                        json!({"tool_calls": [{"index": 0, "id": "call_1", "function": {
                            "name": "read_file", "arguments": args}}]})
                    } else {
                        json!({"content": "SHUTDOWN-ANSWER"})
                    };
                    let out = format!(
                        "data: {}\n\ndata: {}\n\ndata: [DONE]\n\n",
                        json!({"choices": [{"index": 0, "delta": delta, "finish_reason": null}]}),
                        json!({"choices": [], "usage": {"prompt_tokens": 1,
                            "completion_tokens": 1, "total_tokens": 2}})
                    );
                    format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\n\
                         Content-Length: {}\r\nConnection: close\r\n\r\n{out}",
                        out.len()
                    )
                } else {
                    let message = if wants_tool {
                        json!({"role": "assistant", "content": null,
                            "tool_calls": [{"id": "call_1", "type": "function",
                                "function": {"name": "read_file", "arguments": args}}]})
                    } else {
                        json!({"role": "assistant", "content": "SHUTDOWN-ANSWER"})
                    };
                    let data = json!({
                        "id": "c", "object": "chat.completion", "created": 1, "model": "mock",
                        "choices": [{"index": 0, "message": message,
                            "finish_reason": if wants_tool { "tool_calls" } else { "stop" }}],
                        "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2}
                    })
                    .to_string();
                    format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n\
                         Content-Length: {}\r\nConnection: close\r\n\r\n{data}",
                        data.len()
                    )
                };
                let _ = s.write_all(resp.as_bytes());
            });
        }
    });
    format!("http://{addr}/v1")
}

// --------------------------------------------------- webhook destination

/// A webhook receiver: counts the requests it has read and, with a gate, holds
/// each one until the test opens it; then answers 200.
fn receiver(gate: Option<Arc<Gate>>) -> (u16, Arc<AtomicUsize>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let hits = Arc::new(AtomicUsize::new(0));
    let h = Arc::clone(&hits);
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let (h, gate) = (Arc::clone(&h), gate.clone());
            std::thread::spawn(move || {
                let mut s = stream;
                let _ = read_json_body(&mut s);
                h.fetch_add(1, Ordering::SeqCst);
                if let Some(g) = gate {
                    g.hold();
                }
                let _ = s.write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                );
            });
        }
    });
    (port, hits)
}

/// A serve workspace whose home has an inbound route that fires the script job
/// `ping`, and the destination `gateway` at `port`; `ping` delivers to it.
struct Hooked {
    dir: PathBuf,
}

impl Hooked {
    fn new(port: u16) -> Self {
        let dir = workspace_dir();
        std::fs::create_dir_all(&dir).unwrap();
        let home = home_of(&dir);
        std::fs::create_dir_all(home.join("tole")).unwrap();
        std::fs::create_dir_all(home.join("w")).unwrap();
        std::fs::write(
            home.join("tole/config.toml"),
            format!(
                r#"
[hooks.gh]
hmac_key_env = "TOLE_TEST_HOOK_KEY"
cron_job = "ping"
events = ["push"]

[webhook_out.gateway]
url = "http://127.0.0.1:{port}/hook"
hmac_key_env = "{SIGN_VAR}"
allow_loopback = true
"#
            ),
        )
        .unwrap();
        cron::create(
            &home.join("cron"),
            CreateSpec {
                id: Some("ping".into()),
                schedule: "@yearly".into(),
                script: Some("printf done".into()),
                workdir: home.join("w"),
                deliver: vec!["webhook:gateway".into()],
                ..CreateSpec::default()
            },
            1,
        )
        .unwrap();
        Hooked { dir }
    }

    fn cron(&self) -> PathBuf {
        home_of(&self.dir).join("cron")
    }

    /// The first serve over this workspace; it removes the directory when dropped.
    fn spawn(&self, base: &str, args: &[&str]) -> Serve {
        self.spawn_in(base, true, args)
    }

    /// A second serve over the same workspace (it never removes the directory).
    fn spawn_again(&self, base: &str) -> Serve {
        self.spawn_in(base, false, &[])
    }

    fn spawn_in(&self, base: &str, own_dir: bool, args: &[&str]) -> Serve {
        let cron = self.cron();
        Serve::spawn_in_env_args(
            base,
            self.dir.clone(),
            own_dir,
            &[
                ("TOLE_CRON_DIR", cron.to_str().unwrap()),
                ("TOLE_TEST_HOOK_KEY", HOOK_KEY),
                (SIGN_VAR, SIGN_KEY),
                ("TZDIR", "/nonexistent"),
            ],
            args,
        )
    }

    /// The webhook delivery rows of the runs of `ping`.
    fn rows(&self) -> Vec<DeliveryStatus> {
        let Ok(l) = store::open_read(&self.cron()) else {
            return Vec::new();
        };
        l.state()
            .dones()
            .filter(|d| d.job_id == "ping")
            .flat_map(|d| {
                l.state()
                    .deliveries(&d.fire_id)
                    .cloned()
                    .collect::<Vec<_>>()
            })
            .filter(|d| d.target == "webhook:gateway")
            .map(|d| d.status)
            .collect()
    }

    /// `outbox.jsonl` holds whole lines only: it ends in a newline and every
    /// line is a JSON object. Returns how many lines it has.
    fn assert_outbox_whole(&self) -> usize {
        let text = std::fs::read_to_string(self.cron().join("outbox.jsonl")).unwrap_or_default();
        assert!(
            text.is_empty() || text.ends_with('\n'),
            "the outbox ends in a half-written line: {text:?}"
        );
        for line in text.lines() {
            let v: Value = serde_json::from_str(line)
                .unwrap_or_else(|e| panic!("a half-written outbox line {line:?}: {e}"));
            assert!(v.is_object(), "{line}");
        }
        text.lines().count()
    }
}

/// A signed inbound delivery that fires `ping`.
fn fire(port: u16, id: &str) -> String {
    let body = br#"{"ref":"refs/heads/main"}"#;
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs()
        .to_string();
    let head = format!(
        "POST /hooks/gh HTTP/1.1\r\nHost: x\r\nContent-Length: {}\r\n\
         X-Tole-Timestamp: {ts}\r\nX-Tole-Signature: {}\r\nX-Tole-Delivery: {id}\r\n\
         X-GitHub-Event: push\r\n\r\n",
        body.len(),
        webhook::sign(HOOK_KEY.as_bytes(), &ts, id, Some("push"), body)
    );
    let mut s = TcpStream::connect(("127.0.0.1", port)).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    s.write_all(head.as_bytes()).unwrap();
    s.write_all(body).unwrap();
    let mut out = String::new();
    let _ = s.read_to_string(&mut out);
    out.lines().next().unwrap_or("").to_string()
}

// ------------------------------------------------------------------ tests

/// The done-when of #1014, for one signal: a turn in flight is detached (its
/// prompt answers `detached`), the process exits 0 within the grace period, the
/// session log parses and holds the detach, the session resumes on a new serve,
/// and the outbox has whole lines only.
fn a_signal_detaches_the_running_turn_and_leaves_everything_resumable(sig: i32) {
    let gate = Gate::new();
    let base = spawn_gated_mock(Arc::clone(&gate));
    let (rport, hits) = receiver(None);
    let hooked = Hooked::new(rport);
    let mut serve = hooked.spawn(&base, &[]);
    let port = serve.port;
    std::fs::write(serve.dir.join("note.txt"), "hello\n").unwrap();

    // A delivered webhook first, so the outbox holds a line to check.
    assert!(fire(port, "e-1").contains("202"));
    wait_until(WAIT, "the webhook receiver to get the delivery", || {
        hits.load(Ordering::SeqCst) == 1
    });
    wait_for(WAIT, "the delivery's ledger row", || {
        hooked.rows().into_iter().next()
    });
    assert!(hooked.assert_outbox_whole() >= 1, "the outbox has a line");

    let id = new_session(&serve);
    let (code, body, signalled) = std::thread::scope(|sc| {
        let turn = sc.spawn(|| {
            raw_http(
                port,
                "POST",
                &format!("/sessions/{id}/prompt"),
                r#"{"text":"HOLD the turn"}"#,
            )
        });
        gate.wait_entered();
        let signalled = Instant::now();
        signal(&serve.child, sig);
        wait_draining(port);
        assert!(
            serve.child.try_wait().unwrap().is_none(),
            "a running turn is waited for, not cut"
        );
        gate.open();
        let (code, body) = turn.join().unwrap().expect("the prompt got an answer");
        (code, body, signalled)
    });
    assert_eq!(code, 200, "{body}");
    assert_eq!(body["stopReason"], "detached", "{body}");

    let status = wait_exit(&mut serve.child, GRACE).expect("serve exits within the grace period");
    assert!(
        signalled.elapsed() < GRACE,
        "exited after {:?}",
        signalled.elapsed()
    );
    assert_eq!(status.code(), Some(0), "a clean drain exits 0: {status:?}");

    // The session log parses, and says the turn was detached.
    let file = serve.session_file(&id);
    let text = std::fs::read_to_string(&file).unwrap();
    for line in text.lines() {
        serde_json::from_str::<Value>(line)
            .unwrap_or_else(|e| panic!("a half-written session line {line:?}: {e}"));
    }
    assert!(text.contains("owner detached the turn"), "{text}");
    tole_core::storage::JsonlStorage::open_read_only(&file).expect("the log opens");
    hooked.assert_outbox_whole();

    // A new serve over the same workspace resumes it and answers a new prompt.
    let again = hooked.spawn_again(&base);
    let (code, body) = again.http(
        "POST",
        "/sessions",
        Some(&json!({"resume": id}).to_string()),
    );
    assert_eq!(code, 200, "{body}");
    prompt(&again, &id, "and now, resumed");
}

#[test]
fn sigterm_detaches_the_running_turn_and_leaves_everything_resumable() {
    a_signal_detaches_the_running_turn_and_leaves_everything_resumable(libc::SIGTERM);
}

#[test]
fn sigint_does_the_same() {
    a_signal_detaches_the_running_turn_and_leaves_everything_resumable(libc::SIGINT);
}

#[test]
fn a_delivery_attempt_in_flight_is_finished_not_abandoned() {
    let gate = Gate::new();
    let (rport, hits) = receiver(Some(Arc::clone(&gate)));
    let hooked = Hooked::new(rport);
    let mut serve = hooked.spawn("http://127.0.0.1:9/v1", &[]);
    let port = serve.port;

    assert!(fire(port, "e-1").contains("202"));
    gate.wait_entered();
    assert_eq!(hits.load(Ordering::SeqCst), 1, "one attempt, in flight");

    signal(&serve.child, libc::SIGTERM);
    wait_draining(port);
    assert!(
        serve.child.try_wait().unwrap().is_none(),
        "the sender's attempt in flight is waited for"
    );
    gate.open();
    let status = wait_exit(&mut serve.child, GRACE).expect("serve exits within the grace period");
    assert_eq!(status.code(), Some(0), "{status:?}");

    // The attempt settled: its row says sent, and the outbox is whole.
    assert_eq!(hooked.rows(), vec![DeliveryStatus::Sent]);
    assert!(hooked.assert_outbox_whole() >= 1);
}

#[test]
fn an_idle_serve_exits_promptly_with_code_0() {
    let mut serve = Serve::spawn("http://127.0.0.1:9/v1");
    let started = Instant::now();
    signal(&serve.child, libc::SIGTERM);
    let status = wait_exit(&mut serve.child, GRACE).expect("serve exits");
    assert_eq!(status.code(), Some(0), "{status:?}");
    assert!(
        started.elapsed() < Duration::from_secs(10),
        "an idle serve took {:?} to exit",
        started.elapsed()
    );
}

#[test]
fn a_second_signal_does_not_wait_for_the_turn() {
    let gate = Gate::new();
    let base = spawn_gated_mock(Arc::clone(&gate));
    let mut serve = Serve::spawn(&base);
    let port = serve.port;
    std::fs::write(serve.dir.join("note.txt"), "hello\n").unwrap();
    let id = new_session(&serve);
    let turn = std::thread::spawn(move || {
        raw_http(
            port,
            "POST",
            &format!("/sessions/{id}/prompt"),
            r#"{"text":"HOLD forever"}"#,
        )
    });
    gate.wait_entered();
    signal(&serve.child, libc::SIGTERM);
    wait_draining(port);
    assert!(serve.child.try_wait().unwrap().is_none());
    signal(&serve.child, libc::SIGTERM);
    let status = wait_exit(&mut serve.child, GRACE).expect("a second signal ends the wait");
    assert_eq!(status.code(), Some(1), "{status:?}");
    gate.open();
    let _ = turn.join();
}

#[test]
fn a_turn_that_outlives_the_grace_period_is_left_and_the_exit_is_non_zero() {
    let gate = Gate::new();
    let base = spawn_gated_mock(Arc::clone(&gate));
    let mut serve = Serve::spawn_with_args(&base, &["--shutdown-grace", "1"]);
    let port = serve.port;
    std::fs::write(serve.dir.join("note.txt"), "hello\n").unwrap();
    let id = new_session(&serve);
    let turn = std::thread::spawn(move || {
        raw_http(
            port,
            "POST",
            &format!("/sessions/{id}/prompt"),
            r#"{"text":"HOLD past the grace"}"#,
        )
    });
    gate.wait_entered();
    let signalled = Instant::now();
    signal(&serve.child, libc::SIGTERM);
    let status = wait_exit(&mut serve.child, GRACE).expect("serve gives up after its grace");
    assert_eq!(status.code(), Some(1), "{status:?}");
    assert!(
        signalled.elapsed() >= Duration::from_secs(1),
        "it gave up before the grace period: {:?}",
        signalled.elapsed()
    );
    gate.open();
    let _ = turn.join();
}

#[test]
fn a_bad_grace_value_stops_the_start_up() {
    let dir = TestDir::new("tole-shutdown-badgrace");
    let mut cmd = common::hermetic_command(env!("CARGO_BIN_EXE_tole"), &dir, &[]);
    let out = cmd
        .args(["serve", "--token", "t0ken", "--shutdown-grace", "99999"])
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1));
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("shutdown grace"),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// The supervisor's proof that a serve is gone is its `<socket>.lock`: after a
/// clean drain the socket file is removed and the lock can be taken again.
#[test]
fn a_socket_serve_releases_its_socket_and_lock_on_a_clean_drain() {
    use std::os::unix::fs::PermissionsExt;
    let dir = TestDir::in_system_temp("tsd");
    let socket = dir.join("run").join("s.sock");
    std::fs::create_dir_all(dir.join("ws")).unwrap();
    std::fs::create_dir_all(dir.join("home")).unwrap();
    let token_file = dir.join("token");
    std::fs::write(&token_file, "per-launch-token-1014\n").unwrap();
    std::fs::set_permissions(&token_file, std::fs::Permissions::from_mode(0o600)).unwrap();
    let (mut child, _) = common::spawn_listening(|_port| {
        let mut cmd = common::hermetic_command(env!("CARGO_BIN_EXE_tole"), &dir.join("home"), &[]);
        cmd.args(["serve", "--workspace"])
            .arg(dir.join("ws"))
            .current_dir(dir.join("ws"))
            .arg("--socket")
            .arg(&socket)
            .arg("--token-file")
            .arg(&token_file)
            .env("TOLE_BASE_URL", "http://127.0.0.1:9/v1")
            .env("TOLE_MODEL", "mock-model")
            .env("TOLE_API_KEY", "sk-test")
            .stdin(Stdio::null())
            .stdout(Stdio::null());
        cmd
    });
    let lock_path = dir.join("run").join("s.sock.lock");
    assert!(socket.exists(), "the socket is there while serving");
    let lock = std::fs::File::open(&lock_path).unwrap();
    // SAFETY: `flock` on a descriptor this test owns.
    let try_lock = |f: &std::fs::File| unsafe {
        libc::flock(f.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) == 0
    };
    assert!(!try_lock(&lock), "the running serve holds the lock");

    signal(&child, libc::SIGINT);
    let status = wait_exit(&mut child, GRACE).expect("serve exits");
    assert_eq!(status.code(), Some(0), "{status:?}");
    assert!(!socket.exists(), "the socket file is removed");
    assert!(try_lock(&lock), "the lock is released");
}

#[cfg(feature = "mcp-http")]
#[test]
fn the_mcp_transport_drains_and_exits_0_too() {
    let home = TestDir::new("tole-shutdown-mcp");
    let (mut child, _port) = common::spawn_listening(|port| {
        let mut cmd = common::hermetic_command(env!("CARGO_BIN_EXE_tole"), &home, &[]);
        cmd.args([
            "serve",
            "--transport",
            "mcp",
            "--port",
            &port.to_string(),
            "--token",
            "t0ken",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::null());
        cmd
    });
    let started = Instant::now();
    signal(&child, libc::SIGTERM);
    let status = wait_exit(&mut child, GRACE).expect("serve exits");
    assert_eq!(status.code(), Some(0), "{status:?}");
    assert!(started.elapsed() < Duration::from_secs(10));
}
