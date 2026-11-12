//! `POST /hooks/<route>` on the real `tole serve` (#582, part 1): the
//! security gate in front of every webhook. Signed deliveries are accepted
//! once (idempotent across restarts), bad or stale signatures are refused,
//! the body cap applies before the body is read, the per-route rate limit
//! holds, and the route key never appears in any output.

#![cfg(feature = "shell-tools")]

use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use tole_core::webhook;

mod common;

const KEY: &str = "hook-key-for-tests-0123456789abcdef";

struct Serve {
    child: Child,
    port: u16,
}

impl Drop for Serve {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// A private home (user config) and cron dir.
fn setup(tag: &str, hooks: &str) -> PathBuf {
    let base = std::env::temp_dir().join(format!("tole-hooks-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    std::fs::create_dir_all(base.join("home/tole")).unwrap();
    std::fs::write(base.join("home/tole/config.toml"), hooks).unwrap();
    base
}

fn command(base: &Path, port: u16, bind: &str) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_tole"));
    cmd.args(["serve", "--port", &port.to_string(), "--token", "t0ken"])
        .args(["--bind", bind])
        .env("CODECORA_HOME", base.join("home"))
        .env("TOLE_CRON_DIR", base.join("cron"))
        .env("TOLE_TEST_HOOK_KEY", KEY)
        .stdin(Stdio::null())
        .stdout(Stdio::null());
    cmd
}

fn spawn(base: &Path) -> Serve {
    let (child, port) = common::spawn_listening(|port| command(base, port, "127.0.0.1"));
    Serve { child, port }
}

const CONFIG: &str =
    "[hooks.gh]\nhmac_key_env = \"TOLE_TEST_HOOK_KEY\"\nrate_per_minute = 3\nmax_body_bytes = 64\n";

fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64
}

/// Sends a request with `headers`, returns `(status line, body)`.
fn send(port: u16, path: &str, headers: &[(&str, String)], body: &[u8]) -> (String, String) {
    let mut s = TcpStream::connect(("127.0.0.1", port)).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    let mut req = format!(
        "POST {path} HTTP/1.1\r\nHost: x\r\nContent-Length: {}\r\n",
        body.len()
    );
    for (k, v) in headers {
        req.push_str(&format!("{k}: {v}\r\n"));
    }
    req.push_str("\r\n");
    s.write_all(req.as_bytes()).unwrap();
    s.write_all(body).unwrap();
    let mut out = String::new();
    let _ = s.read_to_string(&mut out);
    let status = out.lines().next().unwrap_or("").to_string();
    let body = out.split("\r\n\r\n").nth(1).unwrap_or("").to_string();
    (status, body)
}

/// A native signed delivery of `body` with delivery id `id`.
fn signed(id: &str, ts: i64, body: &[u8]) -> Vec<(&'static str, String)> {
    let ts = ts.to_string();
    vec![
        ("X-Tole-Timestamp", ts.clone()),
        ("X-Tole-Signature", webhook::sign(KEY.as_bytes(), &ts, body)),
        ("X-Tole-Delivery", id.to_string()),
    ]
}

#[test]
fn a_signed_delivery_is_accepted_once_even_across_a_restart() {
    let base = setup("once", CONFIG);
    let body = br#"{"action":"opened"}"#;
    {
        let s = spawn(&base);
        let (st, b) = send(s.port, "/hooks/gh", &signed("d-1", now(), body), body);
        assert!(st.contains("202"), "{st} {b}");
        let (st, b) = send(s.port, "/hooks/gh", &signed("d-1", now(), body), body);
        assert!(st.contains("200") && b.contains("duplicate"), "{st} {b}");
    }
    let s = spawn(&base);
    let (st, b) = send(s.port, "/hooks/gh", &signed("d-1", now(), body), body);
    assert!(
        st.contains("200") && b.contains("duplicate"),
        "after a restart: {st} {b}"
    );
    // GitHub's scheme on the same route.
    let gh = vec![
        (
            "X-Hub-Signature-256",
            format!(
                "sha256={}",
                webhook::hmac_sha256(KEY.as_bytes(), body)
                    .iter()
                    .map(|b| format!("{b:02x}"))
                    .collect::<String>()
            ),
        ),
        ("X-GitHub-Delivery", "gh-1".to_string()),
    ];
    let (st, _) = send(s.port, "/hooks/gh", &gh, body);
    assert!(st.contains("202"), "{st}");
}

#[test]
fn bad_stale_unsigned_or_unknown_deliveries_are_refused() {
    let base = setup("refuse", CONFIG);
    let s = spawn(&base);
    let body = br#"{"a":1}"#;
    let mut bad = signed("d", now(), body);
    bad[1].1 = webhook::sign(b"another-key-another-key-another!", &bad[0].1, body);
    assert!(send(s.port, "/hooks/gh", &bad, body).0.contains("401"));
    let stale = signed("d", now() - 301, body);
    assert!(send(s.port, "/hooks/gh", &stale, body).0.contains("401"));
    assert!(send(s.port, "/hooks/gh", &[], body).0.contains("401"));
    let no_id = &signed("d", now(), body)[..2];
    assert!(send(s.port, "/hooks/gh", no_id, body).0.contains("400"));
    assert!(send(s.port, "/hooks/nope", &signed("d", now(), body), body)
        .0
        .contains("404"));
    // The bearer token is not a webhook credential.
    let bearer = vec![("Authorization", "Bearer t0ken".to_string())];
    assert!(send(s.port, "/hooks/gh", &bearer, body).0.contains("401"));
}

#[test]
fn the_body_cap_applies_before_the_body_is_read() {
    let base = setup("cap", CONFIG);
    let s = spawn(&base);
    // Declares 100 kB but sends none: the answer must not wait for it.
    let mut c = TcpStream::connect(("127.0.0.1", s.port)).unwrap();
    c.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    c.write_all(b"POST /hooks/gh HTTP/1.1\r\nHost: x\r\nContent-Length: 100000\r\n\r\n")
        .unwrap();
    let mut out = String::new();
    let _ = c.read_to_string(&mut out);
    assert!(out.starts_with("HTTP/1.1 413"), "{out}");
    // 65 bytes is over this route's 64.
    let body = [b'x'; 65];
    assert!(
        send(s.port, "/hooks/gh", &signed("big", now(), &body), &body)
            .0
            .contains("413")
    );
}

#[test]
fn the_route_rate_limit_holds() {
    let base = setup("rate", CONFIG);
    let s = spawn(&base);
    let body = b"{}";
    for i in 0..3 {
        let id = format!("r{i}");
        let (st, b) = send(s.port, "/hooks/gh", &signed(&id, now(), body), body);
        assert!(st.contains("202"), "{i}: {st} {b}");
    }
    let (st, _) = send(s.port, "/hooks/gh", &signed("r3", now(), body), body);
    assert!(st.contains("429"), "{st}");
}

#[test]
fn a_route_without_a_key_or_an_open_route_off_loopback_refuses_to_start() {
    for (tag, cfg, bind, want) in [
        (
            "nokey",
            "[hooks.bare]\nrate_per_minute = 3\n",
            "127.0.0.1",
            "bare",
        ),
        (
            "open",
            "[hooks.open]\ninsecure_loopback = true\n",
            "0.0.0.0",
            "loopback",
        ),
    ] {
        let base = setup(tag, cfg);
        let mut child = command(&base, 0, bind)
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        // A server that starts anyway must fail the test, not hang it.
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        let status = loop {
            if let Some(s) = child.try_wait().unwrap() {
                break Some(s);
            }
            if std::time::Instant::now() > deadline {
                let _ = child.kill();
                let _ = child.wait();
                break None;
            }
            std::thread::sleep(Duration::from_millis(50));
        };
        let mut err = String::new();
        if let Some(mut e) = child.stderr.take() {
            let _ = e.read_to_string(&mut err);
        }
        let status = status.unwrap_or_else(|| panic!("{tag}: the server started: {err}"));
        assert!(!status.success(), "{tag}: {err}");
        assert!(err.contains(want), "{tag}: {err}");
    }
}

#[test]
fn the_route_key_never_appears_in_any_output() {
    let base = setup("quiet", CONFIG);
    let body = br#"{"x":1}"#;
    let (child, port) = common::spawn_listening(|port| {
        let mut c = command(&base, port, "127.0.0.1");
        c.stdout(Stdio::piped());
        c
    });
    let s = Serve { child, port };
    let mut answers = String::new();
    for h in [
        signed("q1", now(), body),
        signed("q2", now() - 999, body),
        vec![],
    ] {
        let (st, b) = send(s.port, "/hooks/gh", &h, body);
        answers.push_str(&st);
        answers.push_str(&b);
    }
    drop(s);
    let ledger = std::fs::read_to_string(base.join("cron/jobs.jsonl")).unwrap();
    for text in [answers.as_str(), ledger.as_str()] {
        assert!(!text.contains(KEY), "{text}");
    }
    // A key file is read like the env value, and is not shown either.
    let key_file = base.join("key");
    std::fs::write(&key_file, format!("{KEY}\n")).unwrap();
    let cfg = format!("[hooks.file]\nhmac_key_file = \"{}\"\n", key_file.display());
    std::fs::write(base.join("home/tole/config.toml"), cfg).unwrap();
    let s = spawn(&base);
    let (st, _) = send(s.port, "/hooks/file", &signed("f1", now(), body), body);
    assert!(st.contains("202"), "{st}");
}
