//! `POST /hooks/<route>` fires a cron job (#582, part 2b), on the real
//! `tole serve`: the events allowlist and the filter run before anything
//! fires (`200 ignored`); a match is claimed through the path a tick uses,
//! so a paused job or one that fails the trust check (#640) never runs.

#![cfg(feature = "shell-tools")]

use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use tole_cli::cron::{self, store, CreateSpec};
use tole_core::ledger::RunStatus;
use tole_core::webhook;

mod common;

const KEY: &str = "hook-key-for-fire-tests-0123456789";

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

const CONFIG: &str = r#"
[hooks.gh]
hmac_key_env = "TOLE_TEST_HOOK_KEY"
cron_job = "ping"
events = ["push"]
template = "commit: {head_commit.message}"
[hooks.gh.filter]
path = "ref"
equals = "refs/heads/main"

[hooks.paused]
hmac_key_env = "TOLE_TEST_HOOK_KEY"
cron_job = "sleeper"

[hooks.forged]
hmac_key_env = "TOLE_TEST_HOOK_KEY"
cron_job = "forged"
"#;

/// A home with the routes, a cron dir with the jobs, a workdir.
fn setup(tag: &str) -> PathBuf {
    let base = std::env::temp_dir().join(format!("tole-hookfire-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    std::fs::create_dir_all(base.join("home/tole")).unwrap();
    std::fs::create_dir_all(base.join("w")).unwrap();
    std::fs::write(base.join("home/tole/config.toml"), CONFIG).unwrap();
    let dir = base.join("cron");
    let script = |id: &str, paused: bool| CreateSpec {
        id: Some(id.into()),
        schedule: "@yearly".into(),
        script: Some("echo pong".into()),
        workdir: base.join("w"),
        paused,
        ..CreateSpec::default()
    };
    cron::create(&dir, script("ping", false), 1).unwrap();
    cron::create(&dir, script("sleeper", true), 1).unwrap();
    // A script job written as an agent's: the trust check blocks it.
    let mut forged = cron::create(&dir, script("forged-src", false), 1).unwrap();
    forged.id = "forged".into();
    forged.extra.insert("created_by".into(), "agent".into());
    forged.extra.insert("enabled_by".into(), "cli".into());
    store::with_lock(&dir, |l| Ok(l.put_job(forged)?)).unwrap();
    base
}

fn spawn(base: &Path) -> Serve {
    let (child, port) = common::spawn_listening(|port| {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_tole"));
        cmd.args(["serve", "--port", &port.to_string(), "--token", "t0ken"])
            .env("CODECORA_HOME", base.join("home"))
            .env("TOLE_CRON_DIR", base.join("cron"))
            .env("TOLE_TEST_HOOK_KEY", KEY)
            .stdin(Stdio::null())
            .stdout(Stdio::null());
        cmd
    });
    Serve { child, port }
}

fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64
}

/// A signed delivery of `body` to `route` as `event`: `(status, body)`.
fn deliver(port: u16, route: &str, event: &str, id: &str, body: &[u8]) -> (String, String) {
    let ts = now().to_string();
    let head = format!(
        "POST /hooks/{route} HTTP/1.1\r\nHost: x\r\nContent-Length: {}\r\n\
         X-Tole-Timestamp: {ts}\r\nX-Tole-Signature: {}\r\nX-Tole-Delivery: {id}\r\n\
         X-GitHub-Event: {event}\r\n\r\n",
        body.len(),
        webhook::sign(KEY.as_bytes(), &ts, body)
    );
    let mut s = TcpStream::connect(("127.0.0.1", port)).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    s.write_all(head.as_bytes()).unwrap();
    s.write_all(body).unwrap();
    let mut out = String::new();
    let _ = s.read_to_string(&mut out);
    let status = out.lines().next().unwrap_or("").to_string();
    (
        status,
        out.split("\r\n\r\n").nth(1).unwrap_or("").to_string(),
    )
}

/// Fires of `job` in the ledger.
fn fires(base: &Path, job: &str) -> usize {
    let l = store::open_read(&base.join("cron")).unwrap();
    let n = l.state().dones().filter(|d| d.job_id == job).count();
    n + l.state().pending().filter(|f| f.job_id == job).count()
}

const PUSH: &[u8] = br#"{"ref":"refs/heads/main","head_commit":{"message":"bump version"}}"#;

#[test]
fn a_matching_delivery_fires_the_job_and_others_are_ignored() {
    let base = setup("match");
    let s = spawn(&base);
    // Not an admitted event, then not passing the filter: nothing fires.
    let (st, b) = deliver(s.port, "gh", "issues", "e-1", PUSH);
    assert!(
        st.contains("200") && b.contains("ignored") && b.contains("event"),
        "{st} {b}"
    );
    let other = br#"{"ref":"refs/heads/dev"}"#;
    let (st, b) = deliver(s.port, "gh", "push", "e-2", other);
    assert!(
        st.contains("200") && b.contains("ignored") && b.contains("filter"),
        "{st} {b}"
    );
    let (st, _) = deliver(s.port, "gh", "push", "e-3", b"not json");
    assert!(st.contains("400"), "{st}");
    assert_eq!(fires(&base, "ping"), 0);
    // A match fires the job once and answers with the fire.
    let (st, b) = deliver(s.port, "gh", "push", "e-4", PUSH);
    assert!(st.contains("202") && b.contains("\"fire\""), "{st} {b}");
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let l = store::open_read(&base.join("cron")).unwrap();
        if let Some(d) = l.state().dones().find(|d| d.job_id == "ping") {
            assert_eq!(d.status, RunStatus::Ok);
            break;
        }
        assert!(Instant::now() < deadline, "the fired run never settled");
        std::thread::sleep(Duration::from_millis(100));
    }
    // The same delivery again is a duplicate and fires nothing.
    let (st, _) = deliver(s.port, "gh", "push", "e-4", PUSH);
    assert!(st.contains("200"), "{st}");
    assert_eq!(fires(&base, "ping"), 1);
}

#[test]
fn a_paused_or_untrusted_job_is_not_fired() {
    let base = setup("refuse");
    let s = spawn(&base);
    let (st, b) = deliver(s.port, "paused", "push", "p-1", b"{}");
    assert!(st.contains("409") && b.contains("paused"), "{st} {b}");
    assert_eq!(fires(&base, "sleeper"), 0);
    let (st, _) = deliver(s.port, "forged", "push", "f-1", b"{}");
    assert!(st.contains("409"), "{st}");
    assert_eq!(fires(&base, "forged"), 0);
    let l = store::open_read(&base.join("cron")).unwrap();
    let b = l.state().blocked().find(|b| b.job_id == "forged").unwrap();
    assert_eq!(b.reason, "script_untrusted", "the #640 trust check ran");
}
