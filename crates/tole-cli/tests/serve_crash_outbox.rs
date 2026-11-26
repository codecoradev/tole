//! #1018, fourth slice: SIGKILL the REAL `tole serve` while it delivers an
//! outbound webhook (#911) and check what the outbox file and the receiver
//! see. The event path is: a signed inbound hook fires a cron job, the run
//! finishes, one `enqueue` line is appended to `<cron dir>/outbox.jsonl`, a
//! drainer leases it (`lease`), sends it and records the result (`attempt`,
//! `done`). `TOLE_CRASH_LOOP_N` is the number of rounds of the first test and
//! `TOLE_CRASH_LOOP_SEED` replays its delays (see `crash_loop`).
//!
//! What is pinned, and where the answer is not "exactly once":
//! - Every complete line of the outbox parses after a kill, and the event has
//!   one `enqueue` and, in the end, one `done`.
//! - **Delivery is at least once.** A kill between the send and the `done`
//!   line leaves a lease that runs out (the destination timeout + 5 s) and the
//!   restarted serve sends the event again: the receiver sees the same
//!   `X-Tole-Delivery` id and the same body twice. That id is the receiver's
//!   idempotency key. A kill after the `done` line sends nothing again.
//! - A SIGKILL cannot leave a half-written line: an append is one `write` of
//!   the separator and the whole line. The file can only end in a fragment
//!   after a partial write at the file system (disk full, power loss), so the
//!   test writes one by hand: the fragment is never sent, the next line starts
//!   on a new line of its own, and everything after it parses.
//! - **A hook run killed before it finished is not re-run.** The `fire` claim
//!   stays without a `done`; the same delivery id sent again is answered
//!   `duplicate` and any other id `409 already running` until the next cron
//!   tick turns the claim into an `interrupted` run (`cron/tick.rs`: "never a
//!   re-run"), and no outbox line is written meanwhile. The test pins the
//!   first part only: waiting for the tick takes up to a minute.

#![cfg(all(unix, feature = "shell-tools"))]

mod common;
mod crash_loop;
use common::{ChildGuard, TestDir};
use crash_loop::{rounds, seed, Rng};
use serde_json::Value;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::Path;
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tole_cli::cron::{self, store, CreateSpec};
use tole_cli::webhook_out::{outbox::Line, store::Outbox};
use tole_core::ledger::DeliveryStatus;
use tole_core::webhook;

const HOOK_KEY: &str = "hook-key-for-crash-outbox-0123456789";
const SIGN_KEY: &str = "destination-signing-key-for-crash-0123";
/// How long the receiver holds each answer: a window in which the event is
/// leased and sent but not yet recorded as `done`.
const HOLD: Duration = Duration::from_millis(700);

/// One request the receiver saw: `X-Tole-Delivery` and the body.
type Seen = (String, String);

struct Receiver {
    port: u16,
    seen: Arc<Mutex<Vec<Seen>>>,
}

impl Receiver {
    fn hits(&self) -> Vec<Seen> {
        self.seen.lock().unwrap().clone()
    }
}

/// Records every request when it arrives, answers 200 after `hold`.
fn receiver(hold: Duration) -> Receiver {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let seen = Arc::new(Mutex::new(Vec::new()));
    let log = Arc::clone(&seen);
    std::thread::spawn(move || {
        for s in listener.incoming().flatten() {
            let log = Arc::clone(&log);
            std::thread::spawn(move || {
                let mut s = s;
                let (mut buf, mut chunk) = (Vec::new(), [0u8; 4096]);
                loop {
                    let n = s.read(&mut chunk).unwrap_or(0);
                    if n == 0 {
                        return;
                    }
                    buf.extend_from_slice(&chunk[..n]);
                    let Some(p) = buf.windows(4).position(|w| w == b"\r\n\r\n") else {
                        continue;
                    };
                    let head = String::from_utf8_lossy(&buf[..p]).to_string();
                    let header = |name: &str| {
                        head.lines().find_map(|l| {
                            let (k, v) = l.split_once(':')?;
                            k.eq_ignore_ascii_case(name).then(|| v.trim().to_string())
                        })
                    };
                    let len = header("content-length")
                        .and_then(|v| v.parse().ok())
                        .unwrap_or(0);
                    if buf.len() < p + 4 + len {
                        continue;
                    }
                    let body = String::from_utf8_lossy(&buf[p + 4..p + 4 + len]).into_owned();
                    log.lock()
                        .unwrap()
                        .push((header("x-tole-delivery").unwrap_or_default(), body));
                    std::thread::sleep(hold);
                    let _ = s.write_all(
                        b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                    );
                    return;
                }
            });
        }
    });
    Receiver { port, seen }
}

/// A home with an inbound route that fires job `ping` and the destination
/// `gateway` (timeout 1 s: a lease lasts 6 s), and a cron dir with the job.
fn setup(port: u16) -> TestDir {
    let base = TestDir::new("tole-crash-outbox");
    std::fs::create_dir_all(base.join("home/tole")).unwrap();
    std::fs::create_dir_all(base.join("w")).unwrap();
    std::fs::write(
        base.join("home/tole/config.toml"),
        format!(
            "[hooks.gh]\nhmac_key_env = \"TOLE_TEST_HOOK_KEY\"\ncron_job = \"ping\"\n\
             events = [\"push\"]\n\n[webhook_out.gateway]\nurl = \"http://127.0.0.1:{port}/hook\"\n\
             hmac_key_env = \"PUSHSIG_CRASH\"\nallow_loopback = true\ntimeout_secs = 1\n"
        ),
    )
    .unwrap();
    cron::create(
        &base.join("cron"),
        CreateSpec {
            id: Some("ping".into()),
            schedule: "@yearly".into(),
            script: Some("printf done".into()),
            workdir: base.join("w"),
            deliver: vec!["webhook:gateway".into()],
            ..CreateSpec::default()
        },
        1,
    )
    .unwrap();
    base
}

fn spawn(base: &Path) -> (ChildGuard, u16) {
    let (child, port) = common::spawn_listening(|port| {
        let mut cmd = common::hermetic_command(env!("CARGO_BIN_EXE_tole"), &base.join("home"), &[]);
        cmd.args(["serve", "--port", &port.to_string(), "--token", "t0ken"])
            .env("TOLE_CRON_DIR", base.join("cron"))
            .env("TOLE_TEST_HOOK_KEY", HOOK_KEY)
            .env("PUSHSIG_CRASH", SIGN_KEY)
            .env("TZDIR", "/nonexistent")
            .stdin(Stdio::null())
            .stdout(Stdio::null());
        cmd
    });
    (ChildGuard(child), port)
}

fn kill(child: &mut ChildGuard) {
    let _ = child.kill();
    let _ = child.wait();
}

/// A signed inbound delivery that fires `ping`: `(status line, body)`.
fn fire(port: u16, id: &str) -> (String, String) {
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
    (
        out.lines().next().unwrap_or("").to_string(),
        out.split("\r\n\r\n").nth(1).unwrap_or("").to_string(),
    )
}

/// The outbox as `(parsed lines, unparseable lines, ends without newline)`.
/// A line that does not parse is reported, never trusted.
fn outbox(base: &Path) -> (Vec<Value>, Vec<String>, bool) {
    let raw = std::fs::read_to_string(base.join("cron/outbox.jsonl")).unwrap_or_default();
    let torn = !raw.is_empty() && !raw.ends_with('\n');
    let (mut good, mut bad) = (Vec::new(), Vec::new());
    for line in raw.lines().filter(|l| !l.is_empty()) {
        match serde_json::from_str::<Value>(line) {
            Ok(v) if v["k"].is_string() => good.push(v),
            _ => bad.push(line.to_string()),
        }
    }
    (good, bad, torn)
}

fn count(lines: &[Value], kind: &str) -> usize {
    lines.iter().filter(|l| l["k"] == kind).count()
}

fn sent_rows(base: &Path) -> usize {
    let l = store::open_read(&base.join("cron")).unwrap();
    l.state()
        .dones()
        .flat_map(|d| {
            l.state()
                .deliveries(&d.fire_id)
                .cloned()
                .collect::<Vec<_>>()
        })
        .filter(|d| d.target == "webhook:gateway" && d.status == DeliveryStatus::Sent)
        .count()
}

#[test]
fn a_kill_while_an_event_is_sent_delivers_it_at_least_once_with_the_same_id() {
    let (seed, rounds) = (seed(), rounds(2));
    eprintln!(
        "outbox loop: seed={seed} rounds={rounds} (TOLE_CRASH_LOOP_SEED / TOLE_CRASH_LOOP_N)"
    );
    let mut rng = Rng(seed);
    let r = receiver(HOLD);
    let base = setup(r.port);
    let (mut resent, mut once, mut starved) = (0, 0, false);
    for round in 0..rounds {
        let ctx = format!("round {round}, seed {seed}");
        let before = r.hits().len();
        let (mut first, port) = spawn(&base);
        let (status, body) = fire(port, &format!("e-{round}"));
        assert!(status.contains("202"), "{ctx}: {status} {body}");
        common::wait_until(common::WAIT, "the event to reach the receiver", || {
            r.hits().len() > before
        });
        // The receiver holds its answer for 700 ms. The first two rounds are
        // fixed (inside the hold: leased and sent, no result recorded; then
        // after the `done` line), the rest are anywhere around it.
        let seen_at = std::time::Instant::now();
        if round == 1 {
            common::wait_until(common::WAIT, "the event to be recorded as done", || {
                count(&outbox(&base).0, "done") == round + 1
            });
        }
        let delay = match round {
            0 => rng.below(600),
            1 => rng.below(300),
            _ => rng.below(1200),
        };
        std::thread::sleep(Duration::from_millis(delay));
        // A starved machine can stretch the first round past the hold.
        starved |= round == 0 && seen_at.elapsed() > Duration::from_millis(650);
        kill(&mut first);

        let (lines, bad, torn) = outbox(&base);
        assert!(
            bad.is_empty() && !torn,
            "{ctx}: a line that does not parse: {bad:?}"
        );
        let done_before = count(&lines, "done") == round + 1;
        let (mut second, _) = spawn(&base);
        if done_before {
            once += 1;
            // Recorded: nothing is sent again. (The lease of a settled event
            // is moot; give a drainer the time of one pass to prove it.)
            std::thread::sleep(Duration::from_millis(1500));
            assert_eq!(
                r.hits().len(),
                before + 1,
                "{ctx}: a settled event was sent again"
            );
        } else {
            resent += 1;
            // The lease runs out 6 s after the send; the event goes again.
            common::wait_until(
                Duration::from_secs(40),
                "the event to be sent again",
                || r.hits().len() >= before + 2 && count(&outbox(&base).0, "done") == round + 1,
            );
        }
        let hits = r.hits();
        let ours = &hits[before..];
        assert!(
            ours.iter().all(|h| h == &ours[0]),
            "{ctx}: same id and body: {ours:?}"
        );
        let (lines, bad, torn) = outbox(&base);
        assert!(bad.is_empty() && !torn, "{ctx}: {bad:?}");
        assert_eq!(
            count(&lines, "enqueue"),
            round + 1,
            "{ctx}: one enqueue per event"
        );
        assert_eq!(
            count(&lines, "done"),
            round + 1,
            "{ctx}: one done per event"
        );
        common::wait_until(common::WAIT, "the ledger row", || {
            sent_rows(&base) == round + 1
        });
        kill(&mut second);
    }
    eprintln!("outbox loop: {resent} sent again after the restart, {once} already recorded");
    if rounds >= 2 && !starved {
        assert!(resent >= 1 && once >= 1, "both windows are covered");
    }
}

#[test]
fn a_fragment_at_the_end_of_the_outbox_is_never_sent_and_never_glued_to_the_next_line() {
    let r = receiver(Duration::ZERO);
    let base = setup(r.port);
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;
    let body = |id: &str| {
        format!(
            r#"{{"v":1,"id":"{id}","type":"cron_run_finished","destination":"gateway","at":{now},"data":{{}}}}"#
        )
    };
    let ob = Outbox::new(&base.join("cron"));
    ob.append(&[Line::enqueue("gateway", "p-1", &body("p-1"), now)])
        .unwrap();
    // What a partial write leaves: half of a second event, no newline.
    let full = String::from_utf8(
        tole_cli::webhook_out::outbox::encode(&Line::enqueue("gateway", "p-2", &body("p-2"), now))
            .unwrap(),
    )
    .unwrap();
    let fragment = &full[..full.len() / 2];
    let mut f = std::fs::OpenOptions::new()
        .append(true)
        .open(base.join("cron/outbox.jsonl"))
        .unwrap();
    f.write_all(fragment.as_bytes()).unwrap();
    drop(f);
    let (_, bad, torn) = outbox(&base);
    assert_eq!((bad.len(), torn), (1, true), "the fixture is one fragment");

    let (mut serve, port) = spawn(&base);
    common::wait_until(common::WAIT, "p-1 to be delivered", || {
        r.hits().iter().any(|(id, _)| id == "p-1")
    });
    // The next writer is serve itself: a hook run enqueues behind the fragment.
    let (status, body_) = fire(port, "e-9");
    assert!(status.contains("202"), "{status} {body_}");
    common::wait_until(common::WAIT, "the hook event to be delivered", || {
        r.hits().len() >= 2
    });
    common::wait_until(common::WAIT, "its ledger row", || sent_rows(&base) == 1);
    kill(&mut serve);

    let hits = r.hits();
    assert_eq!(hits.len(), 2, "p-2 (the fragment) is never sent: {hits:?}");
    assert!(hits.iter().all(|(id, _)| id != "p-2"), "{hits:?}");
    let (lines, bad, _) = outbox(&base);
    assert_eq!(
        bad,
        [fragment],
        "only the fragment is bad, and nothing is glued to it"
    );
    assert_eq!(count(&lines, "enqueue"), 2, "p-1 and the hook event");
    assert_eq!(count(&lines, "done"), 2);
}

#[test]
fn a_hook_run_killed_before_it_finished_is_not_run_again_and_writes_nothing_half_done() {
    let r = receiver(Duration::ZERO);
    let base = setup(r.port);
    let (mut first, port) = spawn(&base);
    let (status, body) = fire(port, "e-lost");
    assert!(status.contains("202"), "{status} {body}");
    kill(&mut first);

    let (lines, bad, torn) = outbox(&base);
    assert!(bad.is_empty() && !torn, "{bad:?}");
    let (mut second, port) = spawn(&base);
    // A cron run takes tens of milliseconds; a kill right after the 202 comes
    // first. A machine fast enough to have enqueued by then is the other branch.
    if lines.is_empty() {
        // The claim is still pending: the same delivery id is a duplicate (no
        // run), a new one is refused while the job reads as running.
        let (status, body) = fire(port, "e-lost");
        assert!(
            status.contains("200") && body.contains("duplicate"),
            "{status} {body}"
        );
        let (status, body) = fire(port, "e-next");
        assert!(
            status.contains("409") && body.contains("already running"),
            "{status} {body}"
        );
        std::thread::sleep(Duration::from_millis(1500));
        assert!(
            r.hits().is_empty(),
            "the lost run was not run again: {:?}",
            r.hits()
        );
        let (lines, bad, torn) = outbox(&base);
        assert!(
            lines.is_empty() && bad.is_empty() && !torn,
            "nothing was written"
        );
    } else {
        common::wait_until(common::WAIT, "the event to be delivered", || {
            !r.hits().is_empty()
        });
    }
    kill(&mut second);
}
