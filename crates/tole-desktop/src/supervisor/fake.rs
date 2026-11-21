//! A stand-in for `tole serve` inside the test process: it listens on a Unix
//! socket and answers `GET /health` and, for the right bearer token,
//! `GET /sessions`. It holds the serve lock like the real one. It is not a
//! process of its own, so a test never asks the supervisor to signal it.

use std::io::{Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::Duration;

use super::sys;

/// Held by every test that writes a script and then runs it. A child forked by
/// another test thread between the script being written and being executed
/// keeps the file open for writing, and the exec then fails with ETXTBSY; with
/// the tests that start processes taking turns, no fork can fall in that gap.
pub fn spawn_guard() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    LOCK.lock().unwrap_or_else(|p| p.into_inner())
}

pub struct FakeServe {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
    _lock: std::fs::File,
}

/// A fresh private directory for one test, removed on drop.
pub struct TestDir(pub PathBuf);

impl TestDir {
    pub fn new(tag: &str) -> TestDir {
        static N: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let n = N.fetch_add(1, Ordering::Relaxed);
        // Short: a socket address holds about 100 bytes.
        let dir = std::env::temp_dir().join(format!("tds-{tag}-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
        TestDir(dir)
    }

    pub fn path(&self, name: &str) -> PathBuf {
        self.0.join(name)
    }
}

impl Drop for TestDir {
    fn drop(&mut self) {
        let _ = std::fs::set_permissions(&self.0, std::fs::Permissions::from_mode(0o700));
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

impl FakeServe {
    /// Listen in `dir` (created `0700` when missing) as a serve that reports
    /// `protocol` and `version` and accepts `token`.
    pub fn start(dir: &Path, token: &str, protocol: Option<u32>, version: &str) -> FakeServe {
        sys::prepare_private_dir(dir).unwrap();
        let socket = dir.join("serve.sock");
        let lock_path = dir.join("serve.sock.lock");
        let lock = sys::open_lock_file(&lock_path, true).unwrap();
        assert!(sys::try_lock(&lock).unwrap());
        let listener = UnixListener::bind(&socket).unwrap();
        std::fs::set_permissions(&socket, std::fs::Permissions::from_mode(0o600)).unwrap();
        listener.set_nonblocking(true).unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let health = match protocol {
            Some(p) => format!(r#"{{"status":"ok","protocol":{p},"version":"{version}"}}"#),
            None => r#"{"status":"ok"}"#.to_owned(),
        };
        let expected = format!("Bearer {token}");
        let thread = {
            let stop = Arc::clone(&stop);
            std::thread::spawn(move || {
                while !stop.load(Ordering::Relaxed) {
                    match listener.accept() {
                        Ok((conn, _)) => answer(conn, &health, &expected),
                        Err(_) => std::thread::sleep(Duration::from_millis(5)),
                    }
                }
            })
        };
        FakeServe {
            stop,
            thread: Some(thread),
            _lock: lock,
        }
    }

    /// A serve over the socket file a dead one left behind (protocol 1).
    pub fn start_over(dir: &Path, token: &str, version: &str) -> FakeServe {
        let _ = std::fs::remove_file(dir.join("serve.sock"));
        FakeServe::start(dir, token, Some(super::probe::EXPECTED_PROTOCOL), version)
    }

    /// Stop listening, as a serve that died would (the socket file stays).
    pub fn stop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

impl Drop for FakeServe {
    fn drop(&mut self) {
        self.stop();
    }
}

fn answer(mut conn: UnixStream, health: &str, expected_auth: &str) {
    let _ = conn.set_nonblocking(false);
    let _ = conn.set_read_timeout(Some(Duration::from_secs(2)));
    let mut head = Vec::new();
    let mut byte = [0u8; 1];
    while !head.ends_with(b"\r\n\r\n") && head.len() < 8192 {
        match conn.read(&mut byte) {
            Ok(1) => head.push(byte[0]),
            _ => return,
        }
    }
    let head = String::from_utf8_lossy(&head);
    let (status, body) = if head.starts_with("GET /health ") {
        ("200 OK", health.to_owned())
    } else if head.starts_with("GET /sessions ") {
        let authorised = head
            .lines()
            .any(|l| l.eq_ignore_ascii_case(&format!("authorization: {expected_auth}")));
        if authorised {
            ("200 OK", r#"{"sessions":[]}"#.to_owned())
        } else {
            ("401 Unauthorized", r#"{"error":"unauthorized"}"#.to_owned())
        }
    } else {
        ("404 Not Found", r#"{"error":"not found"}"#.to_owned())
    };
    let reply = format!(
        "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\
         Connection: close\r\n\r\n{body}",
        body.len()
    );
    let _ = conn.write_all(reply.as_bytes());
}
