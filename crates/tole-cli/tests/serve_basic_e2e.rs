//! #742 HTTP Basic auth over the REAL `tole serve` (first slice: Basic only,
//! one user, many devices; the static bearer token of #682 keeps working).
//!
//! The password is stored as a PBKDF2 hash that comes from a file or the
//! environment; one `Authenticator` decides every route (REST, SSE, uploads,
//! MCP over HTTP); a wrong user and a wrong password get the same 401 with a
//! `WWW-Authenticate` challenge; failures are rate-limited per IP; webhooks
//! keep their own signature gate; and no credential reaches a log or an error.

#![cfg(feature = "shell-tools")]

use serde_json::{json, Value};
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::Duration;
use tole_cli::auth::PasswordHash;

mod common;
mod serve_harness;
use common::TestDir;

const USER: &str = "owner";
const PASSWORD: &str = "correct horse battery 0742";
const TOKEN: &str = "bearer-token-0742-abcdef";
const HOOK_KEY: &str = "hook-key-for-tests-0123456789abcdef";
/// The floor of `MIN_ITERATIONS`: the tests make a few dozen requests.
const ITERATIONS: u32 = 10_000;

fn hash_of(password: &str) -> String {
    PasswordHash::create(password.as_bytes(), ITERATIONS).unwrap()
}

fn basic(user: &str, password: &str) -> String {
    use base64::Engine;
    format!(
        "Authorization: Basic {}",
        base64::engine::general_purpose::STANDARD.encode(format!("{user}:{password}"))
    )
}

fn good() -> String {
    basic(USER, PASSWORD)
}

struct Resp {
    status: u16,
    head: String,
    body: String,
}

impl Resp {
    fn header(&self, name: &str) -> Option<&str> {
        self.head.lines().skip(1).find_map(|l| {
            let (k, v) = l.split_once(':')?;
            k.eq_ignore_ascii_case(name).then_some(v.trim())
        })
    }
    fn json(&self) -> Value {
        serde_json::from_str(&self.body).unwrap_or(Value::Null)
    }
}

/// One request; `auth` is a whole header line (or empty). Reads the head and
/// then the `Content-Length` body (an event stream has none and is not read).
fn send_raw(port: u16, method: &str, path: &str, auth: &str, body: &[u8]) -> Resp {
    let mut s = TcpStream::connect(("127.0.0.1", port)).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(20))).unwrap();
    let auth = if auth.is_empty() {
        String::new()
    } else {
        format!("{auth}\r\n")
    };
    let req = format!(
        "{method} {path} HTTP/1.1\r\nHost: x\r\n{auth}Content-Type: application/json\r\n\
         Content-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    s.write_all(req.as_bytes()).unwrap();
    s.write_all(body).unwrap();
    read_resp(&mut s)
}

fn read_resp(s: &mut TcpStream) -> Resp {
    let mut buf = Vec::new();
    let mut byte = [0u8; 1];
    while !buf.ends_with(b"\r\n\r\n") {
        if s.read(&mut byte).unwrap_or(0) == 0 {
            break;
        }
        buf.push(byte[0]);
    }
    let head = String::from_utf8_lossy(&buf).to_string();
    let status = head
        .split_whitespace()
        .nth(1)
        .and_then(|c| c.parse().ok())
        .unwrap_or(0);
    let len = head
        .lines()
        .find_map(|l| {
            l.to_ascii_lowercase()
                .strip_prefix("content-length:")
                .and_then(|v| v.trim().parse::<usize>().ok())
        })
        .unwrap_or(0);
    let mut body = vec![0u8; len];
    let _ = s.read_exact(&mut body);
    Resp {
        status,
        head,
        body: String::from_utf8_lossy(&body).to_string(),
    }
}

struct Daemon {
    child: Child,
    port: u16,
    _work: TestDir,
    log: PathBuf,
    hash_file: PathBuf,
    _home: TestDir,
}

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[derive(Default)]
struct Opts<'a> {
    /// Replaces the default `--auth basic ...` arguments when set.
    args: Option<&'a [&'a str]>,
    extra: &'a [&'a str],
    env: &'a [(&'a str, &'a str)],
    config: Option<&'a str>,
}

fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

/// A serve daemon with Basic on by default (hash from a file), its stderr in
/// a file the test reads.
fn spawn(o: Opts) -> Daemon {
    let home = TestDir::new("tole-basic-home");
    let work = TestDir::new("tole-basic-work");
    let cfg_dir = home.join("tole");
    std::fs::create_dir_all(&cfg_dir).unwrap();
    if let Some(c) = o.config {
        std::fs::write(cfg_dir.join("config.toml"), c).unwrap();
    }
    let hash_file = home.join("basic.hash");
    std::fs::write(&hash_file, format!("{}\n", hash_of(PASSWORD))).unwrap();
    let mock = serve_harness::spawn_mock(0);
    let log = home.join("serve.log");
    for _ in 0..5 {
        let port = free_port();
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_tole"));
        cmd.arg("serve")
            .args(["--port", &port.to_string()])
            .args(["--workspace", work.to_str().unwrap()]);
        match o.args {
            Some(a) => {
                cmd.args(a);
            }
            None => {
                cmd.args(["--auth", "basic", "--basic-user", USER])
                    .arg("--basic-hash-file")
                    .arg(&hash_file);
            }
        }
        cmd.args(o.extra)
            .current_dir(work.path())
            .env("TOLE_NO_UPDATE_CHECK", "1")
            .env("CODECORA_HOME", home.path())
            .env("HOME", home.path())
            .env("TOLE_CRON_DIR", home.join("cron"))
            .env("TOLE_BASE_URL", &mock)
            .env("TOLE_MODEL", "mock-model")
            .env("TOLE_API_KEY", "sk-test")
            .env("TOLE_STREAM", "0")
            .env("TOLE_TEST_HOOK_KEY", HOOK_KEY)
            .env_remove("TOLE_SERVE_TOKEN")
            .env_remove("TOLE_SERVE_BASIC_USER")
            .env_remove("TOLE_SERVE_BASIC_HASH")
            .envs(o.env.iter().copied())
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(std::fs::File::create(&log).unwrap());
        let mut child = cmd.spawn().expect("spawn tole");
        let up = common::poll_until(Duration::from_secs(20), || {
            if child.try_wait().ok().flatten().is_some() {
                return Some(false);
            }
            TcpStream::connect(("127.0.0.1", port)).ok().map(|_| true)
        });
        if up == Some(true) {
            return Daemon {
                child,
                port,
                _work: work,
                log,
                hash_file,
                _home: home,
            };
        }
        let _ = child.kill();
        let _ = child.wait();
    }
    panic!(
        "tole serve did not start: {}",
        std::fs::read_to_string(&log).unwrap_or_default()
    );
}

/// Run `tole serve ...` expecting it to refuse; returns its stderr.
fn refused(args: &[&str], env: &[(&str, &str)]) -> String {
    let home = TestDir::new("tole-basic-refuse");
    let mut child = Command::new(env!("CARGO_BIN_EXE_tole"))
        .arg("serve")
        .args(["--port", &free_port().to_string()])
        .args(args)
        .env("CODECORA_HOME", home.path())
        .env("HOME", home.path())
        .env_remove("TOLE_SERVE_TOKEN")
        .env_remove("TOLE_SERVE_BASIC_USER")
        .env_remove("TOLE_SERVE_BASIC_HASH")
        .envs(env.iter().copied())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    // A server that wrongly starts must fail the test, not hang it.
    let status = common::wait_exit(&mut child, Duration::from_secs(30));
    if status.is_none() {
        let _ = child.kill();
        let _ = child.wait();
    }
    let mut stderr = String::new();
    let _ = child.stderr.take().unwrap().read_to_string(&mut stderr);
    assert!(
        status.is_some_and(|s| !s.success()),
        "it must refuse to start: {stderr}"
    );
    let out = std::process::Output {
        status: status.unwrap(),
        stdout: Vec::new(),
        stderr: stderr.into_bytes(),
    };
    String::from_utf8_lossy(&out.stderr).to_string()
}

impl Daemon {
    fn get(&self, path: &str, auth: &str) -> Resp {
        send_raw(self.port, "GET", path, auth, b"")
    }
    fn post(&self, path: &str, auth: &str, body: &Value) -> Resp {
        send_raw(self.port, "POST", path, auth, body.to_string().as_bytes())
    }
    fn log(&self) -> String {
        std::fs::read_to_string(&self.log).unwrap_or_default()
    }
}

// ---------------------------------------------------------------------------

#[test]
fn the_right_password_opens_every_route_family_and_the_bearer_token_still_works() {
    let d = spawn(Opts {
        env: &[("TOLE_SERVE_TOKEN", TOKEN)],
        ..Opts::default()
    });
    let bearer = format!("Authorization: Bearer {TOKEN}");
    for auth in [good(), bearer] {
        let r = d.get("/sessions", &auth);
        assert_eq!(r.status, 200, "{auth}: {}", r.body);
        let r = d.post("/sessions", &auth, &json!({}));
        assert_eq!(r.status, 200, "{}", r.body);
        let id = r.json()["sessionId"].as_str().unwrap().to_string();
        let r = d.post(
            &format!("/sessions/{id}/prompt"),
            &auth,
            &json!({"text": "hi"}),
        );
        assert_eq!(r.status, 200, "{}", r.body);
        assert_eq!(d.get(&format!("/sessions/{id}"), &auth).status, 200);
        assert_eq!(d.get(&format!("/sessions/{id}/entries"), &auth).status, 200);
        assert_eq!(d.get("/approvals", &auth).status, 200);
        // The event stream: the head says 200 and the stream stays open.
        let r = d.get(&format!("/sessions/{id}/events"), &auth);
        assert_eq!(r.status, 200, "{}", r.head);
    }
    // /health needs nothing.
    assert_eq!(d.get("/health", "").status, 200);
    // A wrong bearer is refused, and carries the Basic challenge.
    let r = d.get("/sessions", "Authorization: Bearer nope-nope-nope");
    assert_eq!(r.status, 401);
    assert!(r.header("www-authenticate").unwrap().starts_with("Basic "));
}

/// Every route refuses a request without a valid credential: no header, a
/// wrong password, a wrong user, a wrong scheme. Chunks of routes run on a
/// fresh daemon so the per-IP failure limiter (10 a minute) never answers
/// instead of the gate.
#[test]
fn no_route_answers_without_a_valid_credential() {
    let routes: &[(&str, &str)] = &[
        ("GET", "/sessions"),
        ("POST", "/sessions"),
        ("GET", "/sessions/s-1"),
        ("GET", "/sessions/s-1/status"),
        ("POST", "/sessions/s-1/prompt"),
        ("POST", "/sessions/s-1/interrupt"),
        ("POST", "/sessions/s-1/cancel"),
        ("GET", "/sessions/s-1/events"),
        ("GET", "/sessions/s-1/entries"),
        ("POST", "/sessions/s-1/images"),
        ("GET", "/sessions/s-1/images/00ff00ff00ff00ff"),
        ("GET", "/approvals"),
        ("POST", "/approvals/apr-1/decision"),
        ("GET", "/cron/runs"),
        ("GET", "/no/such/route"),
        ("DELETE", "/sessions/s-1"),
    ];
    let attempts = [
        String::new(),
        basic(USER, "wrong password 0742"),
        basic("intruder", PASSWORD),
        "Authorization: Digest username=owner".to_string(),
    ];
    // 4 attempts per route: two routes per daemon stays under the cap.
    for chunk in routes.chunks(2) {
        let d = spawn(Opts::default());
        for (method, path) in chunk {
            for auth in &attempts {
                let r = send_raw(d.port, method, path, auth, b"{}");
                assert_eq!(r.status, 401, "{method} {path} [{auth}]: {}", r.body);
                assert!(
                    r.header("www-authenticate")
                        .is_some_and(|c| c.starts_with("Basic realm=")),
                    "{method} {path}: {}",
                    r.head
                );
            }
        }
    }
}

#[test]
fn a_wrong_user_and_a_wrong_password_get_the_same_answer() {
    let d = spawn(Opts::default());
    let wrong_pw = d.get("/sessions", &basic(USER, "wrong password 0742"));
    let wrong_user = d.get("/sessions", &basic("intruder", PASSWORD));
    let both = d.get("/sessions", &basic("intruder", "wrong password 0742"));
    let junk = d.get("/sessions", "Authorization: Basic %%%not-base64");
    for r in [&wrong_pw, &wrong_user, &both, &junk] {
        assert_eq!(r.status, 401);
        assert_eq!(r.head, wrong_pw.head, "headers differ");
        assert_eq!(r.body, wrong_pw.body, "bodies differ");
    }
    assert_eq!(wrong_pw.json(), json!({"error": "unauthorized"}));
}

#[test]
fn failed_attempts_are_limited_per_ip_even_for_the_right_password() {
    let d = spawn(Opts::default());
    for n in 0..10 {
        let r = d.get("/sessions", &basic(USER, &format!("guess number {n} 0742")));
        assert_eq!(r.status, 401, "attempt {n}");
    }
    for auth in [basic(USER, "one more guess 0742"), good()] {
        let r = d.get("/sessions", &auth);
        assert_eq!(r.status, 429, "{auth}: {}", r.body);
        assert!(r.header("www-authenticate").is_none());
    }
    // The limiter runs before everything, /health included, as it does for
    // the token (existing behavior, unchanged).
    assert_eq!(d.get("/health", "").status, 429);
}

#[test]
fn the_oversized_upload_answer_is_no_credential_oracle() {
    let d = spawn(Opts::default());
    let id = d.post("/sessions", &good(), &json!({})).json()["sessionId"]
        .as_str()
        .unwrap()
        .to_string();
    let over = |auth: &str| {
        let mut s = TcpStream::connect(("127.0.0.1", d.port)).unwrap();
        s.set_read_timeout(Some(Duration::from_secs(20))).unwrap();
        let auth = if auth.is_empty() {
            String::new()
        } else {
            format!("{auth}\r\n")
        };
        // Declared 64 MiB, never sent: refused before a byte is read.
        write!(
            s,
            "POST /sessions/{id}/images HTTP/1.1\r\nHost: x\r\n{auth}Content-Length: 67108864\r\n\
             Connection: close\r\n\r\n"
        )
        .unwrap();
        read_resp(&mut s)
    };
    assert_eq!(
        over(&good()).status,
        413,
        "only the right password gets 413"
    );
    for auth in [basic(USER, "wrong password 0742"), String::new()] {
        let r = over(&auth);
        assert_eq!(r.status, 401, "{auth}");
        assert!(r.header("www-authenticate").is_some());
    }

    // Between the plain 1 MiB cap and the upload ceiling: only the right
    // password is allowed to have that body read. The others are answered
    // 401 at once (the body is declared and never sent, so a server that
    // waited for it would time out here).
    let mid = |auth: &str, send: usize| {
        let mut s = TcpStream::connect(("127.0.0.1", d.port)).unwrap();
        s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        let auth = if auth.is_empty() {
            String::new()
        } else {
            format!("{auth}\r\n")
        };
        write!(
            s,
            "POST /sessions/{id}/images HTTP/1.1\r\nHost: x\r\n{auth}Content-Length: 3145728\r\n\
             Connection: close\r\n\r\n"
        )
        .unwrap();
        let _ = s.write_all(&vec![0u8; send]);
        read_resp(&mut s)
    };
    for auth in [basic(USER, "wrong password 0742"), String::new()] {
        assert_eq!(mid(&auth, 0).status, 401, "{auth}");
    }
    // A non-upload route over the plain cap: 413 only for a valid credential.
    let big = |auth: &str| {
        let mut s = TcpStream::connect(("127.0.0.1", d.port)).unwrap();
        s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        let auth = if auth.is_empty() {
            String::new()
        } else {
            format!("{auth}\r\n")
        };
        write!(
            s,
            "POST /sessions HTTP/1.1\r\nHost: x\r\n{auth}Content-Length: 3145728\r\n\
             Connection: close\r\n\r\n"
        )
        .unwrap();
        read_resp(&mut s)
    };
    assert_eq!(big(&good()).status, 413);
    assert_eq!(big(&basic(USER, "wrong password 0742")).status, 401);
    let r = mid(&good(), 3 * 1024 * 1024);
    assert!(
        r.status != 401 && r.status != 413 && r.status != 0,
        "the right password has its 3 MiB body read: {} {}",
        r.status,
        r.body
    );
}

#[test]
fn webhooks_keep_their_own_signature_gate() {
    let cfg = "[hooks.gh]\nhmac_key_env = \"TOLE_TEST_HOOK_KEY\"\nrate_per_minute = 30\n";
    let d = spawn(Opts {
        config: Some(cfg),
        ..Opts::default()
    });
    let body = br#"{"action":"opened"}"#;
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs()
        .to_string();
    let sig = tole_core::webhook::sign(HOOK_KEY.as_bytes(), &ts, "d-742", None, body);
    let hook = |auth: &str, signed: bool| {
        let mut s = TcpStream::connect(("127.0.0.1", d.port)).unwrap();
        s.set_read_timeout(Some(Duration::from_secs(20))).unwrap();
        let auth = if auth.is_empty() {
            String::new()
        } else {
            format!("{auth}\r\n")
        };
        let sign = if signed {
            format!(
                "X-Tole-Timestamp: {ts}\r\nX-Tole-Signature: {sig}\r\nX-Tole-Delivery: d-742\r\n"
            )
        } else {
            String::new()
        };
        write!(
            s,
            "POST /hooks/gh HTTP/1.1\r\nHost: x\r\n{auth}{sign}Content-Length: {}\r\n\
             Connection: close\r\n\r\n",
            body.len()
        )
        .unwrap();
        s.write_all(body).unwrap();
        read_resp(&mut s)
    };
    // A valid Basic credential does not open a webhook.
    assert_eq!(hook(&good(), false).status, 401);
    // A signature alone does, with no Basic credential at all.
    let r = hook("", true);
    assert_eq!(r.status, 202, "{} {}", r.head, r.body);
}

#[test]
fn the_token_mode_is_unchanged() {
    let d = spawn(Opts {
        args: Some(&["--token", TOKEN]),
        ..Opts::default()
    });
    let ok = d.get("/sessions", &format!("Authorization: Bearer {TOKEN}"));
    assert_eq!(ok.status, 200);
    // Basic is not a credential of a token-only server, and its 401 has no
    // challenge (as before).
    let r = d.get("/sessions", &good());
    assert_eq!(r.status, 401);
    assert!(r.header("www-authenticate").is_none(), "{}", r.head);
    let r = d.get("/sessions", "Authorization: Bearer wrong-wrong-wrong");
    assert_eq!(r.status, 401);
    assert!(r.header("www-authenticate").is_none());
}

#[test]
fn start_up_refusals_name_the_remedy_and_never_the_hash() {
    let hash = hash_of(PASSWORD);
    let home = TestDir::new("tole-basic-hash-file");
    let file = home.join("h");
    std::fs::write(&file, &hash).unwrap();
    let f = file.to_str().unwrap();

    // No user, no hash.
    let e = refused(&["--auth", "basic"], &[]);
    assert!(e.contains("needs a user"), "{e}");
    let e = refused(&["--auth", "basic", "--basic-user", USER], &[]);
    assert!(e.contains("needs a password hash"), "{e}");
    // A plaintext password is not a hash; the error never repeats it.
    let e = refused(
        &["--auth", "basic", "--basic-user", USER],
        &[("TOLE_SERVE_BASIC_HASH", PASSWORD)],
    );
    assert!(e.contains("malformed") && !e.contains(PASSWORD), "{e}");
    // A weak hash.
    let weak = hash.replacen("i=10000", "i=1000", 1);
    let e = refused(
        &["--auth", "basic", "--basic-user", USER],
        &[("TOLE_SERVE_BASIC_HASH", &weak)],
    );
    assert!(
        e.contains("too few iterations") && !e.contains(&weak),
        "{e}"
    );
    // No hash flag exists: a hash on the command line is not offered.
    let e = refused(&["--auth", "basic", "--basic-hash", &hash], &[]);
    assert!(e.contains("--basic-hash"), "{e}");
    // A user with ':' cannot be addressed by Basic.
    let e = refused(
        &[
            "--auth",
            "basic",
            "--basic-user",
            "a:b",
            "--basic-hash-file",
            f,
        ],
        &[],
    );
    assert!(e.contains("without ':'"), "{e}");
    // Basic flags without Basic.
    let e = refused(&["--token", TOKEN, "--basic-user", USER], &[]);
    assert!(e.contains("need --auth basic"), "{e}");
    // Token mode still refuses to run without a token.
    let e = refused(&[], &[]);
    assert!(e.contains("unauthenticated"), "{e}");
    // Basic over plain HTTP beyond loopback needs the owner's word.
    let e = refused(
        &[
            "--auth",
            "basic",
            "--basic-user",
            USER,
            "--basic-hash-file",
            f,
            "--bind",
            "0.0.0.0",
        ],
        &[],
    );
    assert!(
        e.contains("--allow-insecure-basic") && e.contains("reverse proxy"),
        "{e}"
    );
}

#[test]
fn the_hash_may_come_from_the_environment_and_a_non_loopback_bind_with_the_flag() {
    let hash = hash_of(PASSWORD);
    let d = spawn(Opts {
        args: Some(&[
            "--auth",
            "basic",
            "--bind",
            "0.0.0.0",
            "--allow-insecure-basic",
        ]),
        env: &[
            ("TOLE_SERVE_BASIC_USER", USER),
            ("TOLE_SERVE_BASIC_HASH", &hash),
        ],
        ..Opts::default()
    });
    assert_eq!(d.get("/sessions", &good()).status, 200);
    assert_eq!(
        d.get("/sessions", &basic(USER, "wrong password 0742"))
            .status,
        401
    );
    // The hash file of the default spawn is unused here.
    assert!(d.hash_file.exists());
}

#[test]
fn no_credential_reaches_the_log_or_the_command_line() {
    use base64::Engine;
    let hash = hash_of(PASSWORD);
    let d = spawn(Opts::default());
    let wrong = "wrong password 0742";
    d.get("/sessions", &good());
    d.get("/sessions", &basic(USER, wrong));
    d.get("/sessions", &basic("intruder", PASSWORD));
    d.get("/sessions", "Authorization: Basic !!!");
    // A body too large for a wrong credential takes the same path.
    d.post("/sessions", &basic(USER, wrong), &json!({"cwd": "/"}));
    let log = d.log();
    let enc =
        |u: &str, p: &str| base64::engine::general_purpose::STANDARD.encode(format!("{u}:{p}"));
    for secret in [
        PASSWORD,
        wrong,
        &enc(USER, PASSWORD),
        &enc(USER, wrong),
        &enc("intruder", PASSWORD),
        &hash,
        hash.rsplit('$').next().unwrap(),
    ] {
        assert!(!log.contains(secret), "the log holds {secret:?}:\n{log}");
    }
    assert!(log.contains("listening on"), "{log}");
    let cmdline = std::fs::read(format!("/proc/{}/cmdline", d.child.id())).unwrap_or_default();
    let cmdline = String::from_utf8_lossy(&cmdline);
    assert!(
        !cmdline.contains(PASSWORD) && !cmdline.contains(&hash),
        "{cmdline}"
    );
}

#[test]
fn serve_hash_makes_a_hash_the_server_accepts_and_never_prints_the_password() {
    let make = |input: &str, extra: &[&str]| {
        let mut child = Command::new(env!("CARGO_BIN_EXE_tole"))
            .arg("serve-hash")
            .args(extra)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(input.as_bytes())
            .unwrap();
        child.wait_with_output().unwrap()
    };
    let out = make(&format!("{PASSWORD}\n"), &["--iterations", "10000"]);
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout).to_string();
    let stderr = String::from_utf8_lossy(&out.stderr).to_string();
    let hash = stdout.trim();
    assert!(hash.starts_with("$pbkdf2-sha256$i=10000$"), "{hash}");
    assert!(!stdout.contains(PASSWORD) && !stderr.contains(PASSWORD));
    // A second run salts differently.
    let again = make(&format!("{PASSWORD}\n"), &["--iterations", "10000"]);
    assert_ne!(String::from_utf8_lossy(&again.stdout).trim(), hash);

    // The server takes that hash.
    let d = spawn(Opts {
        args: Some(&["--auth", "basic", "--basic-user", USER]),
        env: &[("TOLE_SERVE_BASIC_HASH", hash)],
        ..Opts::default()
    });
    assert_eq!(d.get("/sessions", &good()).status, 200);
    assert_eq!(
        d.get("/sessions", &basic(USER, "wrong password 0742"))
            .status,
        401
    );

    // Too short, too weak: refused with a message that never has the input.
    let short = make("tiny\n", &[]);
    assert!(!short.status.success());
    let e = String::from_utf8_lossy(&short.stderr);
    assert!(e.contains("at least 8") && !e.contains("tiny"), "{e}");
    let weak = make(&format!("{PASSWORD}\n"), &["--iterations", "100"]);
    assert!(!weak.status.success());
}

/// The MCP face goes through the same `Authenticator`.
#[cfg(feature = "mcp-http")]
#[test]
fn the_mcp_transport_uses_the_same_check() {
    const INIT: &str = r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"t","version":"1"}}}"#;
    let d = spawn(Opts {
        extra: &["--transport", "mcp"],
        env: &[("TOLE_SERVE_TOKEN", TOKEN)],
        ..Opts::default()
    });
    let post = |auth: &str| {
        let mut s = TcpStream::connect(("127.0.0.1", d.port)).unwrap();
        s.set_read_timeout(Some(Duration::from_secs(20))).unwrap();
        let auth = if auth.is_empty() {
            String::new()
        } else {
            format!("{auth}\r\n")
        };
        write!(
            s,
            "POST /mcp HTTP/1.1\r\nHost: 127.0.0.1:{}\r\n{auth}Content-Type: application/json\r\n\
             Accept: application/json, text/event-stream\r\nContent-Length: {}\r\n\
             Connection: close\r\n\r\n{INIT}",
            d.port,
            INIT.len()
        )
        .unwrap();
        let mut head = Vec::new();
        let mut byte = [0u8; 1];
        while !head.ends_with(b"\r\n\r\n") {
            if s.read(&mut byte).unwrap_or(0) == 0 {
                break;
            }
            head.push(byte[0]);
        }
        String::from_utf8_lossy(&head).to_string()
    };
    for auth in [good(), format!("Authorization: Bearer {TOKEN}")] {
        let head = post(&auth);
        assert!(head.starts_with("HTTP/1.1 200"), "{auth}: {head}");
    }
    for auth in [
        String::new(),
        basic(USER, "wrong password 0742"),
        basic("intruder", PASSWORD),
        "Authorization: Bearer nope-nope-nope".to_string(),
    ] {
        let head = post(&auth);
        assert!(head.starts_with("HTTP/1.1 401"), "{auth}: {head}");
        assert!(
            head.to_ascii_lowercase()
                .contains("www-authenticate: basic realm="),
            "{head}"
        );
    }
    // The limiter holds here too (4 failures so far; 6 more reach the cap).
    for _ in 0..6 {
        assert!(post(&basic(USER, "wrong password 0742")).starts_with("HTTP/1.1 401"));
    }
    assert!(post(&good()).starts_with("HTTP/1.1 429"));
}

/// An upload is authenticated while its headers are read, and the verdict is
/// handed on: a refused upload counts exactly one failure.
#[test]
fn a_refused_upload_counts_one_failure() {
    let d = spawn(Opts::default());
    let wrong = basic(USER, "wrong password 0742");
    for n in 0..10 {
        let r = send_raw(d.port, "POST", "/sessions/s-1/images", &wrong, b"xx");
        assert_eq!(r.status, 401, "attempt {n}");
    }
    let r = send_raw(d.port, "POST", "/sessions/s-1/images", &good(), b"xx");
    assert_eq!(r.status, 429, "ten failures, then limited: {}", r.body);
}
