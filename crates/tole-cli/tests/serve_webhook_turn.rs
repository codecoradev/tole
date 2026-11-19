//! `turn_finished` webhooks on the real `tole serve` (#962): one signed event
//! per settled turn of a serve session, ids and a status word only.
//!
//! Provisional owner decisions D1-D9 (see `webhook_out/turn.rs`) are asserted
//! where they show: `last_seq` (D2), `approval_pending` is emitted (D3),
//! no `profile` (D7), a volatile queue that does not replay after a restart
//! (D1). D4 (`detached` folded into `cancelled`) is a unit test of the mapping:
//! a detach only lands while a tool runs.
//!
//! LEAK CHECK: a marker secret, built at run time, is placed in the
//! environment, the prompt, a tool result, the final answer, the provider's
//! error message and an unknown tool name. It must appear in no delivered body
//! and in no stderr line the notifier prints (`tole: provider failed: ...` is
//! the pre-existing `log_outcome` sink and is not the notifier's). A separate
//! assertion proves the marker really was in the session log, so the check
//! cannot pass vacuously.

#![cfg(feature = "shell-tools")]

mod common;
use common::TestDir;
use serde_json::{json, Value};
use std::collections::VecDeque;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, Instant};

use tole_core::webhook;

const SIGN_VAR: &str = "TURN_E2E_SIGN";
const T: Duration = Duration::from_secs(40);

/// A random hex string, so no secret-shaped literal is in the source.
fn rnd() -> String {
    use std::sync::atomic::AtomicU64;
    static N: AtomicU64 = AtomicU64::new(0);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    format!(
        "{:x}{:x}{:x}",
        nanos,
        std::process::id(),
        N.fetch_add(1, Ordering::Relaxed)
    )
}

struct Got {
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

impl Got {
    fn header(&self, k: &str) -> &str {
        self.headers
            .iter()
            .find(|(n, _)| n.eq_ignore_ascii_case(k))
            .map(|(_, v)| v.as_str())
            .unwrap_or_else(|| panic!("no header {k}"))
    }
    fn json(&self) -> Value {
        serde_json::from_slice(&self.body).unwrap()
    }
    fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).to_string()
    }
}

#[derive(Clone, Copy)]
enum Reply {
    Status(u16),
    /// Hold the connection this long, then answer 200.
    Hang(Duration),
}

/// A webhook receiver the test scripts: an answer per request (200 when the
/// script is empty), and a switch that makes it drop connections.
struct Receiver {
    port: u16,
    rx: mpsc::Receiver<Got>,
    script: Arc<Mutex<VecDeque<Reply>>>,
    down: Arc<AtomicBool>,
}

fn receiver() -> Receiver {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let (tx, rx) = mpsc::channel();
    let script: Arc<Mutex<VecDeque<Reply>>> = Arc::default();
    let down = Arc::new(AtomicBool::new(false));
    let (sc, dn) = (Arc::clone(&script), Arc::clone(&down));
    std::thread::spawn(move || {
        for mut s in listener.incoming().flatten() {
            let (tx, sc, dn) = (tx.clone(), Arc::clone(&sc), Arc::clone(&dn));
            std::thread::spawn(move || {
                if dn.load(Ordering::SeqCst) {
                    return; // killed receiver: the connection just closes
                }
                let mut buf = Vec::new();
                let mut chunk = [0u8; 4096];
                let (head_end, len) = loop {
                    let n = s.read(&mut chunk).unwrap_or(0);
                    if n == 0 {
                        return;
                    }
                    buf.extend_from_slice(&chunk[..n]);
                    if let Some(p) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                        let head = String::from_utf8_lossy(&buf[..p]).to_ascii_lowercase();
                        let len = head
                            .lines()
                            .find_map(|l| l.strip_prefix("content-length:")?.trim().parse().ok())
                            .unwrap_or(0usize);
                        break (p + 4, len);
                    }
                };
                while buf.len() < head_end + len {
                    let n = s.read(&mut chunk).unwrap_or(0);
                    if n == 0 {
                        break;
                    }
                    buf.extend_from_slice(&chunk[..n]);
                }
                let head = String::from_utf8_lossy(&buf[..head_end - 4]).to_string();
                let headers = head
                    .lines()
                    .skip(1)
                    .filter_map(|l| l.split_once(':'))
                    .map(|(k, v)| (k.trim().to_string(), v.trim().to_string()))
                    .collect();
                let body = buf[head_end.min(buf.len())..].to_vec();
                let _ = tx.send(Got { headers, body });
                let code = match sc.lock().unwrap().pop_front() {
                    Some(Reply::Status(c)) => c,
                    Some(Reply::Hang(d)) => {
                        std::thread::sleep(d);
                        200
                    }
                    None => 200,
                };
                let _ = write!(
                    s,
                    "HTTP/1.1 {code} X\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                );
            });
        }
    });
    Receiver {
        port,
        rx,
        script,
        down,
    }
}

impl Receiver {
    fn next(&self) -> Got {
        self.rx.recv_timeout(T).expect("a delivery")
    }
    fn expect_none(&self, within: Duration) {
        if let Ok(g) = self.rx.recv_timeout(within) {
            panic!("unexpected delivery: {}", g.text());
        }
    }
}

/// What the mock provider does, chosen by a word in the latest user message.
/// `MODE_READ` reads a file holding the marker, then answers with it;
/// `MODE_WRITE` asks for a write (needs approval), then answers;
/// `MODE_FAIL` fails with a message holding the marker; `MODE_UNKNOWN` asks for
/// a tool whose name holds the marker; `MODE_SLOW` holds at the gate.
struct Provider {
    url: String,
    gate: Arc<common::Gate>,
}

fn mock_provider(marker: String) -> Provider {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}/v1", listener.local_addr().unwrap());
    let gate = common::Gate::new();
    let g = Arc::clone(&gate);
    std::thread::spawn(move || {
        for mut s in listener.incoming().flatten() {
            let (marker, gate) = (marker.clone(), Arc::clone(&g));
            std::thread::spawn(move || {
                let mut buf = Vec::new();
                let mut chunk = [0u8; 16384];
                let deadline = Instant::now() + Duration::from_secs(10);
                let mut body = Value::Null;
                while Instant::now() < deadline {
                    let n = s.read(&mut chunk).unwrap_or(0);
                    buf.extend_from_slice(&chunk[..n]);
                    if let Some(p) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                        if let Ok(v) = serde_json::from_slice::<Value>(&buf[p + 4..]) {
                            body = v;
                            break;
                        }
                    }
                    if n == 0 {
                        break;
                    }
                }
                let streamed = body["stream"].as_bool() == Some(true);
                let msgs = body["messages"].as_array().cloned().unwrap_or_default();
                let last_user = msgs
                    .iter()
                    .rev()
                    .find(|m| m["role"] == "user")
                    .map(|m| m["content"].to_string())
                    .unwrap_or_default();
                let after_tool = msgs.last().is_some_and(|m| m["role"] == "tool");
                let tool_call = |name: &str, args: Value| {
                    json!({"tool_calls": [{"index": 0, "id": "call_1", "function":
                        {"name": name, "arguments": args.to_string()}}]})
                };
                let reply = if last_user.contains("MODE_FAIL") {
                    let e = json!({"error": {"message": format!("rejected {marker}")}}).to_string();
                    let _ = write!(
                        s,
                        "HTTP/1.1 400 Bad Request\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{e}",
                        e.len()
                    );
                    return;
                } else if last_user.contains("MODE_UNKNOWN") {
                    tool_call(&format!("zz_{marker}"), json!({}))
                } else if last_user.contains("MODE_WRITE") && !after_tool {
                    tool_call("write_file", json!({"path": "out.txt", "content": "x"}))
                } else if last_user.contains("MODE_READ") && !after_tool {
                    tool_call("read_file", json!({"path": "note.txt"}))
                } else {
                    if last_user.contains("MODE_SLOW") {
                        gate.hold();
                    }
                    json!({"content": format!("answer {marker}")})
                };
                let is_tool = reply.get("tool_calls").is_some();
                let payload = if streamed {
                    format!(
                        "data: {}\n\ndata: {}\n\ndata: [DONE]\n\n",
                        json!({"choices": [{"index": 0, "delta": reply, "finish_reason": null}]}),
                        json!({"choices": [], "usage": {"prompt_tokens": 1,
                               "completion_tokens": 1, "total_tokens": 2}})
                    )
                } else {
                    let mut msg = json!({"role": "assistant", "content": null});
                    if is_tool {
                        let call = &reply["tool_calls"][0];
                        msg["tool_calls"] = json!([{"id": "call_1", "type": "function",
                            "function": call["function"]}]);
                    } else {
                        msg["content"] = reply["content"].clone();
                    }
                    json!({"id": "c", "object": "chat.completion", "created": 1,
                        "model": "mock", "choices": [{"index": 0, "message": msg,
                        "finish_reason": if is_tool { "tool_calls" } else { "stop" }}],
                        "usage": {"prompt_tokens": 1, "completion_tokens": 1,
                                  "total_tokens": 2}})
                    .to_string()
                };
                let kind = if streamed {
                    "text/event-stream"
                } else {
                    "application/json"
                };
                let _ = write!(
                    s,
                    "HTTP/1.1 200 OK\r\nContent-Type: {kind}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{payload}",
                    payload.len()
                );
            });
        }
    });
    Provider { url, gate }
}

/// A running `tole serve`, its stderr captured to a file.
struct Serve {
    child: Child,
    port: u16,
    stderr: std::path::PathBuf,
}

impl Drop for Serve {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Serve {
    fn stderr(&self) -> String {
        std::fs::read_to_string(&self.stderr).unwrap_or_default()
    }
    fn http(&self, method: &str, path: &str, body: Option<&str>) -> (u16, Value) {
        http(self.port, method, path, body)
    }
}

struct World {
    base: TestDir,
    key: String,
    marker: String,
}

impl World {
    fn new(tag: &str) -> World {
        let base = TestDir::new(&format!("tole-turnhook-{tag}"));
        std::fs::create_dir_all(base.join("home/tole")).unwrap();
        std::fs::create_dir_all(base.join("w")).unwrap();
        let marker = format!("canary-{}", rnd());
        std::fs::write(base.join("w/note.txt"), format!("secret note {marker}\n")).unwrap();
        World {
            base,
            key: format!("turn-key-{}", rnd()),
            marker,
        }
    }

    fn config(&self, destinations: &str) {
        std::fs::write(self.base.join("home/tole/config.toml"), destinations).unwrap();
    }

    fn dest(name: &str, port: u16, events: &str, extra: &str) -> String {
        format!(
            "[webhook_out.{name}]\nurl = \"http://127.0.0.1:{port}/hook\"\n\
             hmac_key_env = \"{SIGN_VAR}\"\nallow_loopback = true\n{events}{extra}\n"
        )
    }

    fn serve(&self, provider: &str, tag: &str) -> Serve {
        let stderr = self.base.join(format!("serve-{tag}.log"));
        for _ in 0..5 {
            let port = TcpListener::bind("127.0.0.1:0")
                .unwrap()
                .local_addr()
                .unwrap()
                .port();
            let mut cmd = Command::new(env!("CARGO_BIN_EXE_tole"));
            cmd.args(["serve", "--port", &port.to_string(), "--token", "t0ken"])
                .current_dir(self.base.join("w"))
                .env("CODECORA_HOME", self.base.join("home"))
                .env("TOLE_CRON_DIR", self.base.join("cron"))
                .env("TOLE_BASE_URL", provider)
                .env("TOLE_MODEL", "mock-model")
                .env("TOLE_API_KEY", format!("sk-{}", rnd()))
                .env(SIGN_VAR, &self.key)
                // A canary in the environment: not a secret-named variable,
                // so only the code's own discipline keeps it off the wire.
                .env("TURN_E2E_NOTE", &self.marker)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::from(std::fs::File::create(&stderr).unwrap()));
            let child = cmd.spawn().expect("spawn tole");
            let mut serve = Serve {
                child,
                port,
                stderr: stderr.clone(),
            };
            let start = Instant::now();
            while start.elapsed() < Duration::from_secs(20) {
                if serve.stderr().contains("listening on") {
                    return serve;
                }
                if serve.child.try_wait().ok().flatten().is_some() {
                    break;
                }
                std::thread::sleep(Duration::from_millis(50));
            }
        }
        panic!("tole serve did not start");
    }

    fn assert_signed(&self, got: &Got) {
        let id = got.header("X-Tole-Delivery");
        assert_eq!(
            got.header("X-Tole-Signature"),
            webhook::sign(
                self.key.as_bytes(),
                got.header("X-Tole-Timestamp"),
                id,
                Some(got.header("X-Tole-Event")),
                &got.body
            )
        );
        assert_eq!(id, got.json()["id"].as_str().unwrap());
    }

    /// The body carries ids and words only, whatever the event.
    fn assert_clean(&self, got: &Got) {
        let t = got.text();
        assert!(!t.contains(&self.marker), "marker leaked: {t}");
        assert!(!t.contains(&self.key), "key leaked: {t}");
        assert!(!t.contains("note.txt") && !t.contains("out.txt"), "{t}");
        assert!(got.body.len() <= 1024, "{}", got.body.len());
    }

    /// The lines the notifier and sender print never carry the marker, the
    /// key or the URL token. ONLY lines containing "webhook" are asserted for
    /// the marker: the pre-existing `log_outcome` lines (`tole: provider failed:
    /// ...`, `tole: unknown tool '...'`) print the provider message and the tool
    /// name by design (a named older sink, see the design note), so asserting
    /// on them would fail for a reason this change does not own. The tests below
    /// PROVE the marker reached those lines (`assert_marker_reached_log_outcome`),
    /// so the clean-body assertions are not vacuous. The key is asserted absent
    /// from the whole of stderr.
    fn assert_stderr_clean(&self, s: &Serve) {
        let log = s.stderr();
        for line in log.lines().filter(|l| l.contains("webhook")) {
            assert!(!line.contains(&self.marker), "{line}");
            assert!(!line.contains(&self.key), "{line}");
            assert!(!line.contains("/hook"), "{line}");
        }
        assert!(!log.contains(&self.key), "the key is in stderr");
    }
}

impl World {
    /// Proves the marker went through the real code path to the pre-existing
    /// `log_outcome` stderr line `needle` (so a body built from the same text
    /// would have carried it), then that no "webhook" line has it.
    fn assert_marker_reached_log_outcome(&self, s: &Serve, needle: &str) {
        let start = Instant::now();
        loop {
            let log = s.stderr();
            if log
                .lines()
                .any(|l| l.contains(needle) && l.contains(&self.marker))
            {
                break;
            }
            assert!(
                start.elapsed() < Duration::from_secs(10),
                "the marker never reached the `{needle}` line, so the check would be vacuous: {log}"
            );
            std::thread::sleep(Duration::from_millis(100));
        }
        for line in s.stderr().lines().filter(|l| l.contains("webhook")) {
            assert!(!line.contains(&self.marker), "{line}");
        }
    }
}

fn http(port: u16, method: &str, path: &str, body: Option<&str>) -> (u16, Value) {
    let mut s = TcpStream::connect(("127.0.0.1", port)).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(60))).unwrap();
    let b = body.unwrap_or("");
    write!(
        s,
        "{method} {path} HTTP/1.1\r\nAuthorization: Bearer t0ken\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{b}",
        b.len()
    )
    .unwrap();
    let mut out = String::new();
    let _ = s.read_to_string(&mut out);
    let code = out
        .split_whitespace()
        .nth(1)
        .and_then(|c| c.parse().ok())
        .unwrap_or(0);
    let payload = out.split("\r\n\r\n").nth(1).unwrap_or("");
    (
        code,
        serde_json::from_str(payload.trim()).unwrap_or(Value::Null),
    )
}

fn new_session(s: &Serve) -> String {
    let (_, v) = s.http("POST", "/sessions", Some("{}"));
    v["sessionId"].as_str().unwrap().to_string()
}

fn prompt(s: &Serve, sid: &str, text: &str) -> (u16, Value) {
    s.http(
        "POST",
        &format!("/sessions/{sid}/prompt"),
        Some(&json!({ "text": text }).to_string()),
    )
}

/// `lastSeq` and the raw log text, read through the authenticated route.
fn log_of(s: &Serve, sid: &str) -> (u64, String) {
    let (code, v) = s.http(
        "GET",
        &format!("/sessions/{sid}/entries?after=0&limit=500"),
        None,
    );
    assert_eq!(code, 200);
    (v["lastSeq"].as_u64().unwrap(), v.to_string())
}

fn data_keys(ev: &Value) -> Vec<String> {
    let mut k: Vec<String> = ev["data"].as_object().unwrap().keys().cloned().collect();
    k.sort();
    k
}

#[test]
fn a_completed_turn_is_announced_once_to_the_destination_that_listed_it_and_leaks_nothing() {
    let w = World::new("done");
    let wanted = receiver();
    let bystander = receiver();
    w.config(&format!(
        "{}{}",
        World::dest("gateway", wanted.port, "events = [\"turn_finished\"]\n", ""),
        // An existing destination (default events) must not start receiving it.
        World::dest("legacy", bystander.port, "", ""),
    ));
    let prov = mock_provider(w.marker.clone());
    let s = w.serve(&prov.url, "a");
    let sid = new_session(&s);
    let (code, p) = prompt(&s, &sid, &format!("MODE_READ read the note {}", w.marker));
    assert_eq!(code, 200, "{p}");
    assert_eq!(p["stopReason"], "end_turn", "{p}");

    let got = wanted.next();
    assert_eq!(got.header("X-Tole-Event"), "turn_finished");
    w.assert_signed(&got);
    w.assert_clean(&got);
    let ev = got.json();
    assert_eq!(ev["v"], 1);
    assert_eq!(ev["type"], "turn_finished");
    assert_eq!(ev["destination"], "gateway");
    assert_eq!(
        data_keys(&ev),
        ["last_seq", "session_id", "status"],
        "D2, D7"
    );
    assert_eq!(ev["data"]["status"], "completed");
    assert_eq!(ev["data"]["session_id"], sid.as_str());
    // D2: the position is the `seq` the entries route pages by.
    let (last_seq, log) = log_of(&s, &sid);
    assert_eq!(ev["data"]["last_seq"], last_seq);
    assert!(ev["id"].as_str().unwrap().starts_with("turn:"));
    assert!(ev["id"]
        .as_str()
        .unwrap()
        .ends_with(&format!(":{last_seq}:1")));
    // Not vacuous: the marker was in the prompt, the tool result and the answer.
    assert!(log.matches(&w.marker).count() >= 3, "{log}");
    // One event, and none for the destination that did not list it.
    wanted.expect_none(Duration::from_secs(3));
    bystander.expect_none(Duration::from_secs(1));
    w.assert_stderr_clean(&s);
    // The status read is still there for a receiver that wants it.
    let (c, _) = s.http("GET", &format!("/sessions/{sid}/status"), None);
    assert_eq!(c, 200);
}

#[test]
fn an_approval_stop_and_the_resumed_turn_are_announced_in_order() {
    let w = World::new("approval");
    let r = receiver();
    w.config(&World::dest(
        "gateway",
        r.port,
        "events = [\"turn_finished\", \"approval_requested\", \"approval_resolved\"]\n",
        "",
    ));
    let prov = mock_provider(w.marker.clone());
    let s = w.serve(&prov.url, "a");
    let sid = new_session(&s);
    let (_, p) = prompt(&s, &sid, &format!("MODE_WRITE {}", w.marker));
    assert_eq!(p["stopReason"], "approval_required", "{p}");
    let approval_id = p["approval"]["id"].as_str().unwrap().to_string();

    let mut first = [r.next(), r.next()];
    first.sort_by_key(|g| g.header("X-Tole-Event").to_string());
    assert_eq!(first[0].header("X-Tole-Event"), "approval_requested");
    assert_eq!(first[1].header("X-Tole-Event"), "turn_finished");
    let pending = first[1].json();
    w.assert_signed(&first[1]);
    w.assert_clean(&first[1]);
    assert_eq!(pending["data"]["status"], "approval_pending");
    assert_eq!(pending["data"]["approval_id"], approval_id.as_str(), "D3");
    assert_eq!(
        data_keys(&pending),
        ["approval_id", "last_seq", "session_id", "status"]
    );
    let seq1 = pending["data"]["last_seq"].as_u64().unwrap();

    // Allow: approval_resolved, then the resumed turn's own end, with a larger
    // position: the receiver never needs a status read.
    let (code, _) = s.http(
        "POST",
        &format!("/approvals/{approval_id}/decision"),
        Some(&json!({"decision": "allow"}).to_string()),
    );
    assert!(code == 200 || code == 202);
    let mut second = [r.next(), r.next()];
    second.sort_by_key(|g| g.header("X-Tole-Event").to_string());
    assert_eq!(second[0].header("X-Tole-Event"), "approval_resolved");
    assert_eq!(second[1].header("X-Tole-Event"), "turn_finished");
    w.assert_signed(&second[1]);
    w.assert_clean(&second[1]);
    let done = second[1].json();
    assert_eq!(done["data"]["status"], "completed");
    assert!(done["data"]["last_seq"].as_u64().unwrap() > seq1);
    assert_ne!(done["id"], pending["id"], "a new leg is a new event");
    assert_eq!(
        done["data"]["last_seq"].as_u64().unwrap(),
        log_of(&s, &sid).0
    );
    r.expect_none(Duration::from_secs(2));
    w.assert_stderr_clean(&s);
}

#[test]
fn failed_blocked_and_cancelled_turns_carry_a_status_and_a_code_never_text() {
    let w = World::new("words");
    let r = receiver();
    w.config(&World::dest(
        "gateway",
        r.port,
        "events = [\"turn_finished\"]\n",
        "",
    ));
    let prov = mock_provider(w.marker.clone());
    let s = w.serve(&prov.url, "a");

    // A provider failure whose message holds the marker.
    let sid = new_session(&s);
    prompt(&s, &sid, &format!("MODE_FAIL {}", w.marker));
    let got = r.next();
    w.assert_signed(&got);
    w.assert_clean(&got);
    let ev = got.json();
    assert_eq!(ev["data"]["status"], "failed");
    assert_eq!(ev["data"]["code"], "provider");
    w.assert_marker_reached_log_outcome(&s, "provider failed");
    assert_eq!(data_keys(&ev), ["code", "last_seq", "session_id", "status"]);

    // A tool the model made up, named after the marker.
    let sid2 = new_session(&s);
    prompt(&s, &sid2, &format!("MODE_UNKNOWN {}", w.marker));
    let got = r.next();
    w.assert_signed(&got);
    w.assert_clean(&got);
    let ev = got.json();
    assert_eq!(ev["data"]["status"], "blocked");
    assert_eq!(ev["data"]["code"], "unknown_tool");
    w.assert_marker_reached_log_outcome(&s, "unknown tool");
    assert_eq!(ev["data"]["session_id"], sid2.as_str());

    // The owner cancels a turn held at the provider.
    let sid3 = new_session(&s);
    let port = s.port;
    let (sid_c, text) = (sid3.clone(), format!("MODE_SLOW {}", w.marker));
    let turn = std::thread::spawn(move || {
        http(
            port,
            "POST",
            &format!("/sessions/{sid_c}/prompt"),
            Some(&json!({ "text": text }).to_string()),
        )
    });
    prov.gate.wait_entered();
    let (c, _) = s.http("POST", &format!("/sessions/{sid3}/cancel"), Some("{}"));
    assert_eq!(c, 200);
    prov.gate.open();
    let (_, p) = turn.join().unwrap();
    assert_eq!(p["stopReason"], "cancelled", "{p}");
    let got = r.next();
    w.assert_signed(&got);
    w.assert_clean(&got);
    let ev = got.json();
    assert_eq!(ev["data"]["status"], "cancelled");
    assert_eq!(data_keys(&ev), ["last_seq", "session_id", "status"]);

    // A busy session and an unknown session announce nothing.
    let (c, _) = prompt(&s, "nope", "hello");
    assert_ne!(c, 200);
    r.expect_none(Duration::from_secs(2));
    w.assert_stderr_clean(&s);
}

#[test]
fn a_500_is_retried_with_the_same_id_and_body_and_a_new_timestamp() {
    let w = World::new("retry");
    let r = receiver();
    r.script.lock().unwrap().push_back(Reply::Status(500));
    w.config(&World::dest(
        "gateway",
        r.port,
        "events = [\"turn_finished\"]\n",
        "",
    ));
    let prov = mock_provider(w.marker.clone());
    let s = w.serve(&prov.url, "a");
    let sid = new_session(&s);
    prompt(&s, &sid, "MODE_READ go");
    let a = r.next();
    let b = r.next();
    w.assert_signed(&a);
    w.assert_signed(&b);
    assert_eq!(a.body, b.body);
    assert_eq!(a.header("X-Tole-Delivery"), b.header("X-Tole-Delivery"));
    assert_ne!(a.header("X-Tole-Timestamp"), b.header("X-Tole-Timestamp"));
    r.expect_none(Duration::from_secs(3));
    w.assert_stderr_clean(&s);
}

#[test]
fn a_400_is_final_and_reported_by_destination_name_only() {
    let w = World::new("final");
    let r = receiver();
    r.script.lock().unwrap().push_back(Reply::Status(400));
    w.config(&World::dest(
        "gateway",
        r.port,
        "events = [\"turn_finished\"]\n",
        "",
    ));
    let prov = mock_provider(w.marker.clone());
    let s = w.serve(&prov.url, "a");
    let sid = new_session(&s);
    prompt(&s, &sid, "MODE_READ go");
    r.next();
    r.expect_none(Duration::from_secs(5));
    assert!(
        s.stderr()
            .contains("webhook event to `gateway` failed: http_400"),
        "{}",
        s.stderr()
    );
    w.assert_stderr_clean(&s);
}

#[test]
fn a_hung_receiver_does_not_delay_the_turn() {
    let w = World::new("hang");
    let r = receiver();
    r.script
        .lock()
        .unwrap()
        .push_back(Reply::Hang(Duration::from_secs(12)));
    w.config(&World::dest(
        "gateway",
        r.port,
        "events = [\"turn_finished\"]\n",
        "timeout_secs = 1\n",
    ));
    let prov = mock_provider(w.marker.clone());
    let s = w.serve(&prov.url, "a");
    let sid = new_session(&s);
    let start = Instant::now();
    let (code, p) = prompt(&s, &sid, "MODE_READ go");
    assert_eq!((code, &p["stopReason"]), (200, &json!("end_turn")));
    assert!(
        start.elapsed() < Duration::from_secs(10),
        "{:?}",
        start.elapsed()
    );
    // The next turn still completes while the first delivery is stuck.
    let (code, _) = prompt(&s, &sid, "MODE_READ again");
    assert_eq!(code, 200);
}

#[test]
fn a_killed_receiver_that_comes_back_still_gets_the_event() {
    let w = World::new("killed");
    let r = receiver();
    r.down.store(true, Ordering::SeqCst);
    w.config(&World::dest(
        "gateway",
        r.port,
        "events = [\"turn_finished\"]\n",
        "",
    ));
    let prov = mock_provider(w.marker.clone());
    let s = w.serve(&prov.url, "a");
    let sid = new_session(&s);
    let (code, _) = prompt(&s, &sid, "MODE_READ go");
    assert_eq!(code, 200, "a dead receiver never fails the turn");
    std::thread::sleep(Duration::from_millis(500));
    r.down.store(false, Ordering::SeqCst);
    let got = r.next();
    w.assert_signed(&got);
    assert_eq!(got.json()["data"]["status"], "completed");
}

#[test]
fn provisional_d1_a_restarted_serve_does_not_replay_a_lost_event() {
    let w = World::new("restart");
    let r = receiver();
    r.down.store(true, Ordering::SeqCst);
    w.config(&World::dest(
        "gateway",
        r.port,
        "events = [\"turn_finished\"]\n",
        "",
    ));
    let prov = mock_provider(w.marker.clone());
    let sid;
    {
        let s = w.serve(&prov.url, "a");
        sid = new_session(&s);
        prompt(&s, &sid, "MODE_READ go");
        // Killed with the event still queued (volatile, option A of the note).
    }
    r.down.store(false, Ordering::SeqCst);
    let s2 = w.serve(&prov.url, "b");
    r.expect_none(Duration::from_secs(6));
    // The receiver reconciles through the authenticated reads instead.
    let (c, _) = s2.http("GET", &format!("/sessions/{sid}/entries?after=0"), None);
    assert!(c == 200 || c == 404, "{c}");
}
