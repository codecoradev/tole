//! `tole webhook test <name>` (#911) on the real binary: one signed
//! `webhook_test` event to a destination of the user config, the status
//! printed, and neither the URL nor the secret anywhere in the output.

#![cfg(feature = "shell-tools")]

mod common;
use common::TestDir;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::Path;
use std::process::{Command, Output};
use std::sync::mpsc;
use std::time::Duration;

const KEY: &str = "webhook-test-cmd-key-0123456789ab";

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

/// Answers every request with `status` and reports what it received.
fn receiver(status: u16) -> (u16, mpsc::Receiver<Got>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
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
            let _ = write!(
                s,
                "HTTP/1.1 {status} X\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
            );
            let _ = tx.send(Got { headers, body });
        }
    });
    (port, rx)
}

/// A home whose user config declares destination `gateway` at `port`.
fn setup(tag: &str, port: u16) -> TestDir {
    let base = TestDir::new(&format!("tole-webhooktest-{tag}"));
    std::fs::create_dir_all(base.join("home/tole")).unwrap();
    std::fs::create_dir_all(base.join("proj/.tole")).unwrap();
    std::fs::write(
        base.join("home/tole/config.toml"),
        format!(
            "model = \"m\"\n\n[webhook_out.gateway]\nurl = \"http://127.0.0.1:{port}/hook?token=url-token-9f8e7d\"\n\
             hmac_key_env = \"TOLE_WEBHOOK_TEST_KEY_VAR\"\nallow_loopback = true\n"
        ),
    )
    .unwrap();
    std::fs::write(base.join("proj/.tole/config.toml"), "model = \"p\"\n").unwrap();
    base
}

fn tole(base: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_tole"))
        .args(args)
        .current_dir(base.join("proj"))
        .env("CODECORA_HOME", base.join("home"))
        .env("TOLE_CRON_DIR", base.join("cron"))
        .env("TOLE_WEBHOOK_TEST_KEY_VAR", KEY)
        .env("TZDIR", "/nonexistent")
        .output()
        .unwrap()
}

fn text(o: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&o.stdout),
        String::from_utf8_lossy(&o.stderr)
    )
}

#[test]
fn a_test_event_is_delivered_signed_and_reported() {
    let (port, rx) = receiver(204);
    let base = setup("ok", port);
    let o = tole(&base, &["webhook", "test", "gateway"]);
    assert!(o.status.success(), "{}", text(&o));
    let out = text(&o);
    assert!(out.contains("delivered, HTTP 204"), "{out}");

    let got = rx.recv_timeout(Duration::from_secs(5)).unwrap();
    assert!(rx.try_recv().is_err(), "exactly one request");
    let body: serde_json::Value = serde_json::from_slice(&got.body).unwrap();
    assert_eq!(body["type"], "webhook_test");
    assert_eq!(body["destination"], "gateway");
    assert_eq!(body["v"], 1);
    let id = got.header("X-Tole-Delivery");
    assert_eq!(id, body["id"].as_str().unwrap());
    assert!(id.starts_with("test:"));
    assert!(out.contains(id), "the event id is printed: {out}");
    assert_eq!(got.header("X-Tole-Event"), "webhook_test");
    // The receiver can verify it with the native v2 scheme.
    assert_eq!(
        got.header("X-Tole-Signature"),
        tole_core::webhook::sign(
            KEY.as_bytes(),
            got.header("X-Tole-Timestamp"),
            id,
            Some("webhook_test"),
            &got.body
        )
    );
    // Neither the URL (it carries a token) nor the key is printed.
    assert!(
        !out.contains("url-token-9f8e7d") && !out.contains("127.0.0.1"),
        "{out}"
    );
    assert!(!out.contains(KEY), "{out}");
    // A test event is sent directly: nothing was queued or recorded.
    assert!(!base.join("cron/outbox.jsonl").exists());
}

#[test]
fn a_receiver_that_refuses_fails_the_command_without_echoing_anything() {
    let (port, rx) = receiver(500);
    let base = setup("500", port);
    let o = tole(&base, &["webhook", "test", "gateway"]);
    assert!(!o.status.success(), "{}", text(&o));
    let out = text(&o);
    assert!(out.contains("HTTP 500, not 2xx"), "{out}");
    assert!(rx.recv_timeout(Duration::from_secs(5)).is_ok());
    // One request: a test is never retried.
    assert!(rx.recv_timeout(Duration::from_millis(500)).is_err());
    assert!(
        !out.contains("url-token-9f8e7d") && !out.contains(KEY),
        "{out}"
    );
}

#[test]
fn a_closed_port_reports_a_code_not_a_url() {
    // Find a port nothing listens on.
    let port = {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        l.local_addr().unwrap().port()
    };
    let base = setup("closed", port);
    let o = tole(&base, &["webhook", "test", "gateway"]);
    assert!(!o.status.success(), "{}", text(&o));
    let out = text(&o);
    assert!(out.contains("not delivered: transport"), "{out}");
    assert!(
        !out.contains("url-token-9f8e7d") && !out.contains("127.0.0.1"),
        "{out}"
    );
}

#[test]
fn an_unknown_name_lists_the_known_ones_and_sends_nothing() {
    let (port, rx) = receiver(200);
    let base = setup("unknown", port);
    let o = tole(&base, &["webhook", "test", "nope"]);
    assert!(!o.status.success());
    let out = text(&o);
    assert!(
        out.contains("no webhook destination `nope`") && out.contains("gateway"),
        "{out}"
    );
    assert!(
        rx.recv_timeout(Duration::from_millis(500)).is_err(),
        "nothing was sent"
    );
    assert!(!out.contains("url-token-9f8e7d"), "{out}");
}

#[test]
fn a_name_with_control_characters_cannot_forge_a_line() {
    let (port, _rx) = receiver(200);
    let base = setup("ctrl", port);
    let o = tole(&base, &["webhook", "test", "a\nforged: line\u{1b}[2J"]);
    assert!(!o.status.success());
    let out = text(&o);
    assert!(!out.contains('\u{1b}'), "{out:?}");
    assert!(
        !out.lines().any(|l| l.starts_with("forged")),
        "a forged line: {out:?}"
    );
}

#[test]
fn a_missing_secret_is_reported_without_sending() {
    let (port, rx) = receiver(200);
    let base = setup("nokey", port);
    let o = Command::new(env!("CARGO_BIN_EXE_tole"))
        .args(["webhook", "test", "gateway"])
        .current_dir(base.join("proj"))
        .env("CODECORA_HOME", base.join("home"))
        .env("TOLE_CRON_DIR", base.join("cron"))
        .env_remove("TOLE_WEBHOOK_TEST_KEY_VAR")
        .env("TZDIR", "/nonexistent")
        .output()
        .unwrap();
    assert!(!o.status.success());
    let out = text(&o);
    assert!(out.contains("not set") || out.contains("secret"), "{out}");
    assert!(rx.recv_timeout(Duration::from_millis(500)).is_err());
}

#[test]
fn no_destinations_is_an_error_that_says_where_to_declare_one() {
    let base = TestDir::new("tole-webhooktest-none");
    std::fs::create_dir_all(base.join("home/tole")).unwrap();
    std::fs::create_dir_all(base.join("proj")).unwrap();
    std::fs::write(base.join("home/tole/config.toml"), "model = \"m\"\n").unwrap();
    let o = tole(&base, &["webhook", "test", "gateway"]);
    assert!(!o.status.success());
    assert!(text(&o).contains("[webhook_out."), "{}", text(&o));
}
