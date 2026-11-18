//! Outbound webhooks on the real `tole serve` (#911): a cron run fired by an
//! inbound hook is delivered to a receiver over HTTP, signed, with the ledger
//! row that says what happened, and the failure modes behave: a 500 is retried
//! with the same id and body, a 4xx is final, a receiver that never answers
//! does not slow the server, a redirect is not followed, a restart picks the
//! event up again, a forged outbox line is not sent, and a destination that
//! cannot load stops the server.
//!
//! The receiver is a thread in this test that checks nothing on trust: the
//! signature is recomputed over the bytes it received.
//!
//! **Leak check (#781):** a destination key (set through a neutral variable
//! name, so it is protected by being registered and not by a naming heuristic)
//! and a token in the destination URL are printed by the job itself, in halves.
//! Neither may appear in what the receiver got, the outbox, the ledger, or the
//! server's stderr.

#![cfg(feature = "shell-tools")]

mod common;
use common::TestDir;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{mpsc, Arc};
use std::time::{Duration, Instant};

use tole_cli::cron::{self, store, CreateSpec};
use tole_core::ledger::DeliveryStatus;
use tole_core::webhook;

const HOOK_KEY: &str = "hook-key-for-serve-webhook-out-0123";
const SIGN_KEY: &str = "destination-signing-key-9f8e7d6c5b4a";
const URL_TOKEN: &str = "url-token-5a4b3c2d1e0f9a8b";
const SIGN_VAR: &str = "PUSHSIG_E2E_A";

// ---------------------------------------------------------------- receiver

/// What the receiver does with one request.
#[derive(Clone)]
enum Act {
    Status(u16),
    /// Read the request, then hold the connection for this long.
    Hang(Duration),
    /// Answer 302 to this URL.
    Redirect(String),
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
}

struct Receiver {
    port: u16,
    rx: mpsc::Receiver<Got>,
    hits: Arc<AtomicUsize>,
}

/// Follows `script` one act per request, then answers 200.
fn receiver(script: Vec<Act>) -> Receiver {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let (tx, rx) = mpsc::channel();
    let hits = Arc::new(AtomicUsize::new(0));
    let script = Arc::new(std::sync::Mutex::new(script.into_iter()));
    let h = Arc::clone(&hits);
    std::thread::spawn(move || {
        for s in listener.incoming().flatten() {
            let (tx, script, h) = (tx.clone(), Arc::clone(&script), Arc::clone(&h));
            std::thread::spawn(move || serve_one(s, tx, script, h));
        }
    });
    Receiver { port, rx, hits }
}

fn serve_one(
    mut s: TcpStream,
    tx: mpsc::Sender<Got>,
    script: Arc<std::sync::Mutex<std::vec::IntoIter<Act>>>,
    hits: Arc<AtomicUsize>,
) {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 4096];
    let (head_end, len) = loop {
        let n = s.read(&mut chunk).unwrap_or(0);
        if n == 0 {
            return;
        }
        buf.extend_from_slice(&chunk[..n]);
        if let Some(p) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            let head = String::from_utf8_lossy(&buf[..p]).to_string();
            let len = head
                .lines()
                .find_map(|l| {
                    l.to_ascii_lowercase()
                        .strip_prefix("content-length:")
                        .and_then(|v| v.trim().parse::<usize>().ok())
                })
                .unwrap_or(0);
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
    hits.fetch_add(1, Ordering::SeqCst);
    let act = script.lock().unwrap().next().unwrap_or(Act::Status(200));
    let _ = tx.send(Got { headers, body });
    match act {
        Act::Status(code) => {
            let _ = write!(
                s,
                "HTTP/1.1 {code} X\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
            );
        }
        Act::Hang(d) => std::thread::sleep(d),
        Act::Redirect(to) => {
            let _ = write!(
                s,
                "HTTP/1.1 302 Found\r\nLocation: {to}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
            );
        }
    }
}

impl Receiver {
    fn next(&self, within: Duration) -> Option<Got> {
        self.rx.recv_timeout(within).ok()
    }
}

// ------------------------------------------------------------------- serve

struct Serve {
    child: Child,
    port: u16,
    stderr: PathBuf,
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
}

/// A home whose user config has an inbound route that fires job `ping` and the
/// destination `gateway` at `port`, with the extra `[webhook_out.gateway]`
/// lines in `extra`; a cron dir where `ping` runs a script that prints the
/// key and the URL token in halves, delivering to `webhook:gateway`.
fn setup(tag: &str, port: u16, extra: &str) -> TestDir {
    let base = TestDir::new(&format!("tole-servewebhook-{tag}"));
    std::fs::create_dir_all(base.join("home/tole")).unwrap();
    std::fs::create_dir_all(base.join("w")).unwrap();
    std::fs::write(
        base.join("home/tole/config.toml"),
        format!(
            r#"
[hooks.gh]
hmac_key_env = "TOLE_TEST_HOOK_KEY"
cron_job = "ping"
events = ["push"]

[webhook_out.gateway]
url = "http://127.0.0.1:{port}/hook?token={URL_TOKEN}"
hmac_key_env = "{SIGN_VAR}"
allow_loopback = true
{extra}
"#
        ),
    )
    .unwrap();
    let split = |v: &str| {
        let (a, b) = v.split_at(v.len() / 2);
        format!("{a} {b}")
    };
    let script = format!("printf done:%s%s {} {}", split(SIGN_KEY), split(URL_TOKEN));
    cron::create(
        &base.join("cron"),
        CreateSpec {
            id: Some("ping".into()),
            schedule: "@yearly".into(),
            script: Some(script),
            workdir: base.join("w"),
            deliver: vec!["webhook:gateway".into()],
            ..CreateSpec::default()
        },
        1,
    )
    .unwrap();
    base
}

fn candidate_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

fn serve_cmd(base: &Path, port: u16, stderr: &Path) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_tole"));
    cmd.args(["serve", "--port", &port.to_string(), "--token", "t0ken"])
        .env("CODECORA_HOME", base.join("home"))
        .env("TOLE_CRON_DIR", base.join("cron"))
        .env("TOLE_TEST_HOOK_KEY", HOOK_KEY)
        .env(SIGN_VAR, SIGN_KEY)
        .env("TZDIR", "/nonexistent")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::from(std::fs::File::create(stderr).unwrap()));
    cmd
}

/// Starts `tole serve` and waits until it listens; its stderr goes to a file.
fn spawn(base: &Path, tag: &str) -> Serve {
    for _ in 0..5 {
        let port = candidate_port();
        let stderr = base.join(format!("serve-{tag}.stderr"));
        let mut child = serve_cmd(base, port, &stderr).spawn().expect("spawn tole");
        let started = Instant::now();
        loop {
            let text = std::fs::read_to_string(&stderr).unwrap_or_default();
            if text.contains("listening on") {
                return Serve {
                    child,
                    port,
                    stderr,
                };
            }
            if child.try_wait().unwrap().is_some() || started.elapsed() > Duration::from_secs(20) {
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        let _ = child.kill();
        let _ = child.wait();
    }
    panic!("tole serve did not start listening");
}

fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64
}

/// A signed inbound delivery that fires `ping`: `(status line, body)`.
fn fire(port: u16, id: &str) -> (String, String) {
    let body = br#"{"ref":"refs/heads/main"}"#;
    let ts = now().to_string();
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
    (
        out.lines().next().unwrap_or("").to_string(),
        out.split("\r\n\r\n").nth(1).unwrap_or("").to_string(),
    )
}

fn health(port: u16) -> Duration {
    let started = Instant::now();
    let mut s = TcpStream::connect(("127.0.0.1", port)).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    s.write_all(b"GET /health HTTP/1.1\r\nConnection: close\r\n\r\n")
        .unwrap();
    let mut out = String::new();
    let _ = s.read_to_string(&mut out);
    assert!(out.contains("200"), "{out}");
    started.elapsed()
}

/// The webhook delivery rows of every run of `ping`.
fn rows(base: &Path) -> Vec<(DeliveryStatus, Option<String>)> {
    let l = store::open_read(&base.join("cron")).unwrap();
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
        .map(|d| (d.status, d.error))
        .collect()
}

fn wait_row(base: &Path) -> (DeliveryStatus, Option<String>) {
    common::wait_for(Duration::from_secs(40), "the webhook ledger row", || {
        rows(base).into_iter().next()
    })
}

fn all_text(base: &Path, serve: &Serve) -> Vec<(&'static str, String)> {
    let read = |p: PathBuf| std::fs::read_to_string(p).unwrap_or_default();
    vec![
        ("outbox", read(base.join("cron/outbox.jsonl"))),
        ("ledger", read(base.join("cron/jobs.jsonl"))),
        ("stderr", serve.stderr()),
    ]
}

fn assert_no_leak(label: &str, text: &str) {
    for (what, secret) in [
        ("the destination key", SIGN_KEY),
        ("the URL token", URL_TOKEN),
        ("the hook key", HOOK_KEY),
    ] {
        assert!(
            !text.contains(secret),
            "{what} leaked into {label}:\n{text}"
        );
    }
}

// ------------------------------------------------------------------- tests

#[test]
fn a_fired_run_is_delivered_by_serve_signed_and_recorded_with_nothing_leaked() {
    let r = receiver(vec![]);
    let base = setup("ok", r.port, "");
    let s = spawn(&base, "a");
    let (st, b) = fire(s.port, "e-1");
    assert!(st.contains("202"), "{st} {b}");

    let got = r.next(Duration::from_secs(30)).expect("a delivery");
    let body: serde_json::Value = serde_json::from_slice(&got.body).unwrap();
    assert_eq!(body["type"], "cron_run_finished");
    assert_eq!(body["destination"], "gateway");
    assert_eq!(body["data"]["job"], "ping");
    assert_eq!(body["data"]["status"], "ok");
    // The job printed the key and the URL token in halves: both were redacted.
    let summary = body["data"]["summary"].as_str().unwrap().to_string();
    assert!(summary.starts_with("done:"), "{summary}");
    assert!(summary.contains("«webhook_out.gateway»"), "{summary}");
    assert!(summary.contains("«webhook_out.gateway.url»"), "{summary}");
    // Signed with the destination key, over exactly the received bytes.
    let id = got.header("X-Tole-Delivery");
    assert_eq!(id, body["id"].as_str().unwrap());
    assert_eq!(got.header("X-Tole-Event"), "cron_run_finished");
    assert_eq!(
        got.header("X-Tole-Signature"),
        webhook::sign(
            SIGN_KEY.as_bytes(),
            got.header("X-Tole-Timestamp"),
            id,
            Some("cron_run_finished"),
            &got.body
        )
    );
    assert!(
        r.next(Duration::from_secs(2)).is_none(),
        "exactly one request"
    );

    assert_eq!(wait_row(&base), (DeliveryStatus::Sent, None));
    // The leak check: nothing the receiver saw, and nothing on disk or on the
    // server's stderr, holds a secret.
    assert_no_leak("the received body", &String::from_utf8_lossy(&got.body));
    for (k, v) in &got.headers {
        assert_no_leak(&format!("header {k}"), v);
    }
    for (label, text) in all_text(&base, &s) {
        assert_no_leak(label, &text);
    }
}

#[test]
fn a_500_is_retried_with_the_same_id_and_body_and_a_new_timestamp() {
    let r = receiver(vec![Act::Status(500)]);
    let base = setup("retry", r.port, "");
    let s = spawn(&base, "a");
    assert!(fire(s.port, "e-1").0.contains("202"));
    let first = r.next(Duration::from_secs(30)).unwrap();
    let second = r.next(Duration::from_secs(30)).expect("the retry");
    assert_eq!(first.body, second.body, "byte-identical body");
    assert_eq!(
        first.header("X-Tole-Delivery"),
        second.header("X-Tole-Delivery")
    );
    assert_ne!(
        first.header("X-Tole-Signature"),
        second.header("X-Tole-Signature"),
        "a retry is signed again"
    );
    assert_eq!(wait_row(&base), (DeliveryStatus::Sent, None));
    assert_eq!(
        rows(&base).len(),
        1,
        "one terminal row, not one per attempt"
    );
}

#[test]
fn a_400_is_final_and_the_server_says_only_the_name_and_the_code() {
    let r = receiver(vec![Act::Status(400)]);
    let base = setup("400", r.port, "");
    let s = spawn(&base, "a");
    assert!(fire(s.port, "e-1").0.contains("202"));
    assert!(r.next(Duration::from_secs(30)).is_some());
    let (status, code) = wait_row(&base);
    assert_eq!(status, DeliveryStatus::Failed);
    assert_eq!(code.as_deref(), Some("http_400"));
    // Never retried.
    assert!(r.next(Duration::from_secs(8)).is_none());
    let err = s.stderr();
    assert!(
        err.contains("webhook delivery to `gateway` failed: http_400"),
        "{err}"
    );
    assert_no_leak("stderr", &err);
}

#[test]
fn a_receiver_that_never_answers_does_not_slow_the_server() {
    let r = receiver(vec![Act::Hang(Duration::from_secs(30)); 3]);
    let base = setup("hang", r.port, "timeout_secs = 1");
    let s = spawn(&base, "a");
    assert!(fire(s.port, "e-1").0.contains("202"));
    assert!(
        r.next(Duration::from_secs(30)).is_some(),
        "the send started"
    );
    // While the sender waits on the receiver, the server answers at once...
    for _ in 0..5 {
        assert!(health(s.port) < Duration::from_secs(2));
    }
    // ...and a second inbound delivery is accepted without waiting for it.
    let started = Instant::now();
    assert!(fire(s.port, "e-2").0.contains("202"));
    assert!(started.elapsed() < Duration::from_secs(5));
    // The first one is not delivered (it times out and waits for a retry).
    assert!(
        rows(&base).is_empty(),
        "no terminal row yet: {:?}",
        rows(&base)
    );
}

#[test]
fn a_redirect_is_not_followed() {
    let elsewhere = receiver(vec![]);
    let r = receiver(vec![Act::Redirect(format!(
        "http://127.0.0.1:{}/stolen",
        elsewhere.port
    ))]);
    let base = setup("redirect", r.port, "");
    let s = spawn(&base, "a");
    assert!(fire(s.port, "e-1").0.contains("202"));
    assert!(r.next(Duration::from_secs(30)).is_some());
    let (status, code) = wait_row(&base);
    assert_eq!(status, DeliveryStatus::Failed);
    assert_eq!(code.as_deref(), Some("redirect_refused"));
    assert_eq!(
        elsewhere.hits.load(Ordering::SeqCst),
        0,
        "the redirect target got a request"
    );
    assert!(elsewhere.next(Duration::from_secs(1)).is_none());
}

#[test]
fn a_closed_port_is_retried_and_does_not_stop_the_server() {
    let port = candidate_port(); // nothing listens here
    let base = setup("closed", port, "");
    let s = spawn(&base, "a");
    assert!(fire(s.port, "e-1").0.contains("202"));
    // The run settled and the event waits for its retry; the server is fine.
    common::wait_for(Duration::from_secs(30), "the run to settle", || {
        let l = store::open_read(&base.join("cron")).unwrap();
        let done = l.state().dones().any(|d| d.job_id == "ping");
        done.then_some(())
    });
    assert!(health(s.port) < Duration::from_secs(2));
    assert!(
        rows(&base).is_empty(),
        "not terminal after one refused connection"
    );
    let outbox = std::fs::read_to_string(base.join("cron/outbox.jsonl")).unwrap();
    assert!(outbox.contains("\"k\":\"enqueue\""), "{outbox}");
    assert_no_leak("stderr", &s.stderr());
}

#[test]
fn a_killed_server_resumes_the_event_after_a_restart_with_the_same_id() {
    let r = receiver(vec![Act::Status(500)]);
    let base = setup("restart", r.port, "");
    let mut s = spawn(&base, "a");
    assert!(fire(s.port, "e-1").0.contains("202"));
    let first = r.next(Duration::from_secs(30)).expect("the first attempt");
    // Killed mid-retry: the event is on disk, not in the dead process.
    let _ = s.child.kill();
    let _ = s.child.wait();
    let s2 = spawn(&base, "b");
    let second = r
        .next(Duration::from_secs(40))
        .expect("sent again after the restart");
    assert_eq!(
        first.header("X-Tole-Delivery"),
        second.header("X-Tole-Delivery")
    );
    assert_eq!(first.body, second.body);
    assert_eq!(wait_row(&base), (DeliveryStatus::Sent, None));
    for (label, text) in all_text(&base, &s2) {
        assert_no_leak(label, &text);
    }
}

#[test]
fn a_forged_outbox_line_is_never_sent() {
    use tole_cli::webhook_out::outbox::Line;
    use tole_cli::webhook_out::store::Outbox;
    let r = receiver(vec![]);
    let base = setup("forged", r.port, "");
    let t = now();
    let body = |dest: &str, id: &str, ty: &str| {
        format!(
            r#"{{"v":1,"id":"{id}","type":"{ty}","destination":"{dest}","at":{t},"data":{{}}}}"#
        )
    };
    let enqueue = |dest: &str, id: &str, body: String| Line::Enqueue {
        dest: dest.into(),
        id: id.into(),
        at: t,
        deadline: t + 21_600,
        body,
    };
    // Lines another process of the account could append before serve starts:
    // a destination that is not configured, a body that names another
    // destination, an event type the destination does not list, an id that
    // is not the body's, and a body over the size cap.
    Outbox::new(&base.join("cron"))
        .append(&[
            enqueue("ghost", "f1", body("ghost", "f1", "cron_run_finished")),
            enqueue("gateway", "f2", body("other", "f2", "cron_run_finished")),
            enqueue("gateway", "f3", body("gateway", "f3", "approval_requested")),
            enqueue("gateway", "f4", body("gateway", "different", "cron_run_finished")),
            enqueue(
                "gateway",
                "f5",
                format!(
                    r#"{{"v":1,"id":"f5","type":"cron_run_finished","destination":"gateway","at":{t},"data":"{}"}}"#,
                    "x".repeat(20_000)
                ),
            ),
        ])
        .unwrap();
    let s = spawn(&base, "a");
    // The sender has judged every configured line (the bad ones are dead).
    let started = Instant::now();
    loop {
        let outbox = std::fs::read_to_string(base.join("cron/outbox.jsonl")).unwrap();
        let dead = outbox.matches("\"k\":\"dead\"").count();
        if dead >= 4 {
            break;
        }
        assert!(
            started.elapsed() < Duration::from_secs(30),
            "not judged: {outbox}"
        );
        std::thread::sleep(Duration::from_millis(200));
    }
    assert_eq!(r.hits.load(Ordering::SeqCst), 0, "a forged line was sent");
    assert!(r.next(Duration::from_secs(1)).is_none());
    assert_no_leak("stderr", &s.stderr());
}

#[test]
fn a_destination_that_cannot_load_stops_the_server_and_names_it() {
    // A private address without `allow_private`.
    let base = TestDir::new("tole-servewebhook-private");
    std::fs::create_dir_all(base.join("home/tole")).unwrap();
    std::fs::write(
        base.join("home/tole/config.toml"),
        format!(
            "[webhook_out.gateway]\nurl = \"https://10.0.0.5/hook?token={URL_TOKEN}\"\n\
             hmac_key_env = \"{SIGN_VAR}\"\n"
        ),
    )
    .unwrap();
    let stderr = base.join("stderr");
    let mut child = serve_cmd(&base, candidate_port(), &stderr).spawn().unwrap();
    let started = Instant::now();
    let status = loop {
        if let Some(s) = child.try_wait().unwrap() {
            break s;
        }
        assert!(
            started.elapsed() < Duration::from_secs(30),
            "serve kept running"
        );
        std::thread::sleep(Duration::from_millis(100));
    };
    assert!(!status.success());
    let err = std::fs::read_to_string(&stderr).unwrap();
    assert!(err.contains("gateway"), "{err}");
    assert!(!err.contains(URL_TOKEN) && !err.contains(SIGN_KEY), "{err}");
}
