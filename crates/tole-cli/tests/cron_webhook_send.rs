//! `webhook:<name>` delivery end to end (#911 part 4): a real tick queues the
//! event, the drain sends it over HTTP to a receiver thread on loopback, and
//! the ledger row appears when it settles. The receiver checks the headers
//! and the signature itself.
#![cfg(feature = "shell-tools")]

mod common;
use common::TestDir;
use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::Path;
use std::sync::{mpsc, Arc};
use std::time::Duration;

use serde_json::Value;
use tole_cli::config::WebhookOutConfig;
use tole_cli::cron::{self, store, webhook, CreateSpec, TickOptions};
use tole_cli::webhook_out::{self, Destinations};
use tole_core::ledger::DeliveryStatus;

/// The tests tick on the real clock: the sender judges an event's age
/// against it, so a synthetic past would make every event expired.
fn now() -> i64 {
    cron::now()
}
const KEY: &str = "tole-send-test-key-0123456789abcdef";

fn tmp(tag: &str) -> TestDir {
    TestDir::new(&format!("tole-webhook-send-{tag}"))
}

/// A request the receiver saw.
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
}

/// Serves `statuses` in order, one per request, then 200s. Returns the port
/// and a channel of what it received.
fn receiver(statuses: Vec<u16>) -> (u16, mpsc::Receiver<Got>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let mut statuses = statuses.into_iter();
        for mut s in listener.incoming().flatten() {
            let mut buf = Vec::new();
            let mut chunk = [0u8; 4096];
            let (head_end, len) = loop {
                let n = s.read(&mut chunk).unwrap_or(0);
                if n == 0 {
                    break (0, 0);
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
            while head_end > 0 && buf.len() < head_end + len {
                let n = s.read(&mut chunk).unwrap_or(0);
                if n == 0 {
                    break;
                }
                buf.extend_from_slice(&chunk[..n]);
            }
            let head = String::from_utf8_lossy(&buf[..head_end.saturating_sub(4)]).to_string();
            let headers = head
                .lines()
                .skip(1)
                .filter_map(|l| l.split_once(':'))
                .map(|(k, v)| (k.trim().to_string(), v.trim().to_string()))
                .collect();
            let body = buf[head_end.min(buf.len())..].to_vec();
            let status = statuses.next().unwrap_or(200);
            let _ = write!(
                s,
                "HTTP/1.1 {status} X\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
            );
            let _ = tx.send(Got { headers, body });
        }
    });
    (port, rx)
}

/// A destination named `gateway` that signs with the variable `var`. The
/// name is neutral on purpose (no "KEY" or "SECRET" in it): the key must be
/// kept out of the output by being registered, not by a naming heuristic.
fn dests_with(port: u16, var: &str) -> Destinations {
    std::env::set_var(var, KEY);
    let cfg = BTreeMap::from([(
        "gateway".to_string(),
        WebhookOutConfig {
            url: Some(format!("http://127.0.0.1:{port}/hook")),
            hmac_key_env: Some(var.into()),
            allow_loopback: Some(true),
            ..WebhookOutConfig::default()
        },
    )]);
    webhook_out::load(&cfg, &|k| std::env::var(k).ok()).unwrap()
}

fn dests(port: u16) -> Destinations {
    dests_with(port, "TOLE_TEST_WEBHOOK_SEND_A")
}

fn job(dir: &Path, id: &str, script: &str) -> CreateSpec {
    CreateSpec {
        id: Some(id.into()),
        schedule: "* * * * *".into(),
        script: Some(script.into()),
        workdir: dir.to_path_buf(),
        deliver: vec!["webhook:gateway".into()],
        ..CreateSpec::default()
    }
}

fn opts(d: &Destinations) -> TickOptions {
    TickOptions {
        webhooks: Some(Arc::new(d.clone())),
        ..TickOptions::standalone(2)
    }
}

fn rows(dir: &Path, fire: &str) -> Vec<(String, DeliveryStatus, Option<String>)> {
    store::open_read(dir)
        .unwrap()
        .state()
        .deliveries(fire)
        .map(|d| (d.target.clone(), d.status, d.error.clone()))
        .collect()
}

#[test]
fn a_run_is_delivered_signed_and_the_ledger_records_sent() {
    let dir = tmp("ok");
    let (port, rx) = receiver(vec![]);
    let d = dests(port);
    cron::create(&dir, job(&dir, "daily", "echo all-good"), now() - 120).unwrap();
    let r = cron::tick(&dir, now(), &opts(&d)).unwrap();
    let fire = r.ran[0].0.clone();
    assert!(
        rows(&dir, &fire).is_empty(),
        "nothing is sent or recorded by the tick itself"
    );
    assert!(rx.try_recv().is_err());

    let stats = webhook::drain(&dir, &d, Duration::from_secs(10)).unwrap();
    assert_eq!(stats.sent, 1, "{stats:?}");
    let got = rx.recv_timeout(Duration::from_secs(5)).unwrap();
    assert!(rx.try_recv().is_err(), "exactly one request");

    let body = got.json();
    assert_eq!(body["v"], 1);
    assert_eq!(body["type"], "cron_run_finished");
    assert_eq!(body["destination"], "gateway");
    assert_eq!(body["data"]["job"], "daily");
    assert_eq!(body["data"]["fire_id"], fire);
    assert_eq!(body["data"]["status"], "ok");
    assert_eq!(body["data"]["summary"].as_str().unwrap().trim(), "all-good");
    assert_eq!(body["data"]["truncated"], false);

    // Headers and the native v2 signature over exactly the received bytes.
    let id = got.header("X-Tole-Delivery");
    assert_eq!(id, body["id"].as_str().unwrap());
    assert_eq!(got.header("X-Tole-Event"), "cron_run_finished");
    assert_eq!(got.header("Content-Type"), "application/json");
    assert!(got.header("User-Agent").starts_with("tole/"));
    assert_eq!(
        got.header("X-Tole-Signature"),
        tole_core::webhook::sign(
            KEY.as_bytes(),
            got.header("X-Tole-Timestamp"),
            id,
            Some("cron_run_finished"),
            &got.body
        )
    );
    let ts: i64 = got.header("X-Tole-Timestamp").parse().unwrap();
    assert!(
        (ts - tole_cli::cron::now()).abs() < 120,
        "a fresh timestamp"
    );

    let rows = rows(&dir, &fire);
    let hook = rows
        .iter()
        .find(|(t, _, _)| t == "webhook:gateway")
        .unwrap();
    assert_eq!((hook.1, hook.2.as_deref()), (DeliveryStatus::Sent, None));
}

#[test]
fn a_500_is_retried_with_the_same_id_and_body() {
    let dir = tmp("retry");
    let (port, rx) = receiver(vec![500, 200]);
    let d = dests(port);
    cron::create(&dir, job(&dir, "j", "echo hi"), now() - 120).unwrap();
    let fire = cron::tick(&dir, now(), &opts(&d)).unwrap().ran[0].0.clone();
    // The first wait is 2 s: inside the budget, so one drain does both. (No
    // new pass starts with less than the destination's timeout left.)
    let stats = webhook::drain(&dir, &d, Duration::from_secs(30)).unwrap();
    assert_eq!((stats.retried, stats.sent), (1, 1), "{stats:?}");
    let first = rx.recv_timeout(Duration::from_secs(5)).unwrap();
    let second = rx.recv_timeout(Duration::from_secs(5)).unwrap();
    assert_eq!(first.body, second.body, "byte-identical body");
    assert_eq!(
        first.header("X-Tole-Delivery"),
        second.header("X-Tole-Delivery")
    );
    assert_eq!(rows(&dir, &fire).len(), 1, "one terminal row, not two");
}

#[test]
fn a_4xx_is_final_and_recorded_failed() {
    let dir = tmp("400");
    let (port, rx) = receiver(vec![400]);
    let d = dests(port);
    cron::create(&dir, job(&dir, "j", "echo hi"), now() - 120).unwrap();
    let fire = cron::tick(&dir, now(), &opts(&d)).unwrap().ran[0].0.clone();
    let stats = webhook::drain(&dir, &d, Duration::from_secs(5)).unwrap();
    assert_eq!(stats.dead, 1, "{stats:?}");
    assert!(rx.recv_timeout(Duration::from_secs(5)).is_ok());
    let rows = rows(&dir, &fire);
    let hook = rows
        .iter()
        .find(|(t, _, _)| t == "webhook:gateway")
        .unwrap();
    assert_eq!(hook.1, DeliveryStatus::Failed);
    assert_eq!(hook.2.as_deref(), Some("http_400"));
    // Final: the outbox holds a `dead` line and nothing pending, so no later
    // drain can send it again.
    let outbox = std::fs::read_to_string(dir.join("outbox.jsonl")).unwrap();
    assert!(outbox.contains("\"k\":\"dead\""), "{outbox}");
    let again = webhook::drain(&dir, &d, Duration::from_secs(1)).unwrap();
    assert_eq!(again.sent + again.retried + again.dead, 0);
    assert!(rx.try_recv().is_err(), "never sent a second time");
}

#[test]
fn a_silent_run_sends_nothing_but_a_failed_run_with_no_output_does() {
    let dir = tmp("silent");
    let (port, rx) = receiver(vec![]);
    let d = dests(port);
    // An ok run with no output: silent, recorded `skipped`.
    cron::create(&dir, job(&dir, "quiet", "true"), now() - 120).unwrap();
    let fire = cron::tick(&dir, now(), &opts(&d)).unwrap().ran[0].0.clone();
    let rows = rows(&dir, &fire);
    assert_eq!(rows[0].1, DeliveryStatus::Skipped, "{rows:?}");
    assert!(!dir.join("outbox.jsonl").exists(), "nothing was queued");

    // A failing run with no output still notifies.
    cron::create(&dir, job(&dir, "broken", "false"), now() - 120).unwrap();
    let r = cron::tick(&dir, now(), &opts(&d)).unwrap();
    let fire = r
        .ran
        .iter()
        .find(|(f, _)| f.starts_with("broken"))
        .unwrap()
        .0
        .clone();
    webhook::drain(&dir, &d, Duration::from_secs(10)).unwrap();
    let got = rx.recv_timeout(Duration::from_secs(5)).unwrap();
    let body = got.json();
    assert_eq!(body["data"]["status"], "error");
    assert_eq!(body["data"]["summary"], "");
    assert_eq!(body["data"]["fire_id"], fire);
}

#[test]
fn a_secret_in_the_output_reaches_neither_the_receiver_nor_the_files() {
    let dir = tmp("leak");
    let (port, rx) = receiver(vec![]);
    // The destination's key and a token in its URL are what a run must not
    // be able to print onto the wire or the disk.
    let d = dests_with(port, "TOLE_TEST_WEBHOOK_SEND_B");
    let canary = "canary-secret-value-9f8e7d6c5b4a";
    tole_core::secrets::add_role_secret("webhook_test_canary", canary);
    // The job's own text must not hold a secret whole (that is the job
    // definition, not output): the run prints each by joining two halves.
    let split = |v: &str| {
        let (a, b) = v.split_at(v.len() / 2);
        format!("{a} {b}")
    };
    let script = format!(
        "printf %s%s%s {} {} {}",
        split(canary),
        split(KEY),
        split(&format!("http://127.0.0.1:{port}/hook"))
    );
    cron::create(&dir, job(&dir, "j", &script), now() - 120).unwrap();
    let fire = cron::tick(&dir, now(), &opts(&d)).unwrap().ran[0].0.clone();
    webhook::drain(&dir, &d, Duration::from_secs(10)).unwrap();
    let got = rx.recv_timeout(Duration::from_secs(5)).unwrap();
    // The run did print them, and the redaction marker shows they were caught.
    let summary = got.json()["data"]["summary"].as_str().unwrap().to_string();
    for marker in [
        "«webhook_test_canary»",
        "«webhook_out.gateway»",
        "«webhook_out.gateway.url»",
    ] {
        assert!(summary.contains(marker), "no {marker} in: {summary}");
    }
    let outbox = std::fs::read_to_string(dir.join("outbox.jsonl")).unwrap();
    let ledger = std::fs::read_to_string(dir.join("jobs.jsonl")).unwrap();
    let all = [
        String::from_utf8_lossy(&got.body).to_string(),
        got.headers
            .iter()
            .filter(|(k, _)| !k.eq_ignore_ascii_case("host"))
            .map(|(k, v)| format!("{k}: {v}"))
            .collect::<Vec<_>>()
            .join("\n"),
        outbox,
        ledger,
    ];
    for (n, text) in all.iter().enumerate() {
        assert!(!text.contains(canary), "the canary leaked into #{n}");
        assert!(!text.contains(KEY), "the destination key leaked into #{n}");
        assert!(
            !text.contains(&format!("http://127.0.0.1:{port}/hook")),
            "the destination URL leaked into #{n}"
        );
    }
    assert!(rows(&dir, &fire)
        .iter()
        .any(|(t, s, _)| t == "webhook:gateway" && *s == DeliveryStatus::Sent));
}

#[test]
fn a_row_is_only_written_for_a_run_that_names_the_destination() {
    let dir = tmp("rows");
    let (port, _rx) = receiver(vec![]);
    let d = dests(port);
    // A job that does NOT deliver to the webhook.
    let mut spec = job(&dir, "plain", "echo hi");
    spec.deliver = vec!["log".into()];
    cron::create(&dir, spec, now() - 120).unwrap();
    let fire = cron::tick(&dir, now(), &opts(&d)).unwrap().ran[0].0.clone();
    // A settled event that claims this run (a forged outbox line).
    let n = webhook::record_settled(
        &dir,
        vec![tole_cli::webhook_out::send::Settled {
            dest: "gateway".into(),
            id: "x".into(),
            fire_id: Some(fire.clone()),
            status: DeliveryStatus::Failed,
            code: Some("bad_line".into()),
        }],
        now(),
    )
    .unwrap();
    assert_eq!(n, 0);
    assert!(rows(&dir, &fire)
        .iter()
        .all(|(t, _, _)| t != "webhook:gateway"));
}

#[test]
fn a_destination_that_is_gone_is_not_sent_to() {
    let dir = tmp("gone");
    let (port, rx) = receiver(vec![]);
    let d = dests(port);
    cron::create(&dir, job(&dir, "j", "echo hi"), now() - 120).unwrap();
    cron::tick(&dir, now(), &opts(&d)).unwrap();
    // The owner removed the destination before it was sent.
    let none = Destinations::default();
    let started = std::time::Instant::now();
    let stats = webhook::drain(&dir, &none, Duration::from_secs(20)).unwrap();
    assert_eq!(stats.sent + stats.retried + stats.dead, 0);
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "a drain with nothing sendable must not wait out its budget: {:?}",
        started.elapsed()
    );
    assert!(rx.try_recv().is_err(), "nothing was sent");
}

#[test]
fn a_terminal_row_is_written_once_however_often_it_is_settled() {
    let dir = tmp("once");
    let (port, _rx) = receiver(vec![]);
    let d = dests(port);
    cron::create(&dir, job(&dir, "j", "echo hi"), now() - 120).unwrap();
    let fire = cron::tick(&dir, now(), &opts(&d)).unwrap().ran[0].0.clone();
    let settled = |status| tole_cli::webhook_out::send::Settled {
        dest: "gateway".into(),
        id: "x".into(),
        fire_id: Some(fire.clone()),
        status,
        code: None,
    };
    let first = webhook::record_settled(&dir, vec![settled(DeliveryStatus::Sent)], now()).unwrap();
    // A slow drainer, a repeated pass or a later reconcile settles it again.
    let again = webhook::record_settled(&dir, vec![settled(DeliveryStatus::Sent)], now()).unwrap();
    assert_eq!((first, again), (1, 0));
    let n = rows(&dir, &fire)
        .iter()
        .filter(|(t, _, _)| t == "webhook:gateway")
        .count();
    assert_eq!(n, 1);
}

#[test]
fn the_serve_sender_sends_when_the_outbox_grows_and_retries_when_due() {
    let dir = tmp("sender");
    let (port, rx) = receiver(vec![500, 200]);
    let d = dests_with(port, "TOLE_TEST_WEBHOOK_SEND_C");
    let mut sender = webhook::Sender::new(&dir);
    // An empty outbox is looked at once, then left alone.
    assert!(sender.step(&d, now(), &mut |_, _| {}));
    assert!(!sender.step(&d, now(), &mut |_, _| {}), "nothing changed");

    cron::create(&dir, job(&dir, "j", "echo hi"), now() - 120).unwrap();
    let fire = cron::tick(&dir, now(), &opts(&d)).unwrap().ran[0].0.clone();
    let mut failed = Vec::new();
    // The file grew: a pass runs and gets the 500; the retry is due in 2 s.
    assert!(sender.step(&d, now(), &mut |d, c| failed
        .push((d.to_string(), c.to_string()))));
    assert!(rx.recv_timeout(Duration::from_secs(5)).is_ok());
    assert!(failed.is_empty(), "a retry is not a failure yet");
    assert!(
        !sender.step(&d, now(), &mut |_, _| {}),
        "no pass before the retry is due"
    );
    // Once it is due, the next look sends it.
    let started = std::time::Instant::now();
    loop {
        sender.step(&d, now(), &mut |_, _| {});
        if rx.try_recv().is_ok() {
            break;
        }
        assert!(started.elapsed() < Duration::from_secs(10), "never retried");
        std::thread::sleep(Duration::from_millis(200));
    }
    let rows = rows(&dir, &fire);
    let hook = rows
        .iter()
        .find(|(t, _, _)| t == "webhook:gateway")
        .unwrap();
    assert_eq!(hook.1, DeliveryStatus::Sent);
}

#[test]
fn the_serve_sender_reports_a_failed_delivery_by_name_and_code_only() {
    let dir = tmp("sender-fail");
    let (port, _rx) = receiver(vec![403]);
    let d = dests_with(port, "TOLE_TEST_WEBHOOK_SEND_D");
    cron::create(&dir, job(&dir, "j", "echo hi"), now() - 120).unwrap();
    cron::tick(&dir, now(), &opts(&d)).unwrap();
    let mut sender = webhook::Sender::new(&dir);
    let mut failed = Vec::new();
    sender.step(&d, now(), &mut |d, c| {
        failed.push((d.to_string(), c.to_string()))
    });
    assert_eq!(
        failed,
        vec![("gateway".to_string(), "http_403".to_string())]
    );
}

#[test]
fn doctor_reports_counts_and_names_but_never_a_url() {
    let dir = tmp("doctor");
    let (port, _rx) = receiver(vec![]);
    let d = dests_with(port, "TOLE_TEST_WEBHOOK_SEND_E");
    // Nothing configured, nothing queued: silent.
    let (lines, ok) = webhook::doctor_lines(&dir, &Destinations::default(), now());
    assert!((lines.is_empty(), ok) == (true, true), "{lines:?}");

    cron::create(&dir, job(&dir, "j", "echo hi"), now() - 120).unwrap();
    cron::tick(&dir, now(), &opts(&d)).unwrap();
    let (lines, ok) = webhook::doctor_lines(&dir, &d, now() + 1200);
    assert!(ok, "{lines:?}");
    let text = lines.join("\n");
    assert!(text.contains("1 pending"), "{text}");
    assert!(text.contains("oldest"), "{text}");
    assert!(!text.contains("127.0.0.1"), "a URL in doctor: {text}");

    // The destination was removed from the config: an error that names it.
    let (lines, ok) = webhook::doctor_lines(&dir, &Destinations::default(), now());
    assert!(!ok);
    assert!(
        lines
            .iter()
            .any(|l| l.contains("webhook:gateway") && l.contains("not configured")),
        "{lines:?}"
    );
}
