//! The client over a Unix socket (#1067): the same deadlines, retry
//! classification and token redaction as over TCP, against scripted servers.
//! (The real `tole serve --socket` is in `tole-cli`'s `client_contract` test.)
#![cfg(unix)]

use std::io::{Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tole_client::{Client, Error, PromptRequest};

const TOKEN: &str = "sekrit-token-1067";

/// A private directory with a socket in it, removed on drop.
struct Sock {
    dir: PathBuf,
    path: PathBuf,
}

impl Drop for Sock {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// Serve `connections` connections with `script`, which gets each one after
/// the request head was read, plus everything that was read.
fn server(
    connections: usize,
    script: impl Fn(UnixStream, &str) + Send + 'static,
) -> (Sock, Arc<AtomicUsize>) {
    static N: AtomicUsize = AtomicUsize::new(0);
    let dir = std::env::temp_dir().join(format!(
        "tcs-{}-{}",
        std::process::id(),
        N.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::create_dir(&dir).unwrap();
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
    let path = dir.join("s.sock");
    let listener = UnixListener::bind(&path).unwrap();
    let hits = Arc::new(AtomicUsize::new(0));
    let seen = Arc::clone(&hits);
    std::thread::spawn(move || {
        for _ in 0..connections {
            let (mut s, _) = listener.accept().unwrap();
            let mut head = [0u8; 2048];
            let n = s.read(&mut head).unwrap_or(0);
            seen.fetch_add(1, Ordering::SeqCst);
            script(s, &String::from_utf8_lossy(&head[..n]));
        }
    });
    (Sock { dir, path }, hits)
}

fn client(sock: &Sock) -> Client {
    Client::connect_unix(&sock.path, Some(TOKEN.into())).unwrap()
}

#[test]
fn a_call_answers_and_sends_the_bearer_token_and_a_fixed_host() {
    let (sock, _) = server(1, |mut s, head| {
        assert!(head.starts_with("GET /health HTTP/1.1\r\n"), "{head}");
        assert!(head.contains(&format!("Authorization: Bearer {TOKEN}\r\n")));
        assert!(head.contains("Host: localhost\r\n"), "{head}");
        s.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 15\r\n\r\n{\"status\":\"ok\"}")
            .unwrap();
    });
    assert_eq!(client(&sock).health().unwrap().status, "ok");
}

#[test]
fn the_token_is_in_neither_debug_nor_an_error() {
    let (sock, _) = server(1, |mut s, _| {
        s.write_all(
            b"HTTP/1.1 401 Unauthorized\r\nContent-Length: 24\r\n\r\n{\"error\":\"unauthorized\"}",
        )
        .unwrap();
    });
    let c = client(&sock);
    assert!(!format!("{c:?}").contains(TOKEN));
    let err = c.sessions().unwrap_err();
    assert!(matches!(err, Error::Status { status: 401, .. }), "{err:?}");
    assert!(!format!("{err:?} {err}").contains(TOKEN));
    // A refused socket never reaches the token either.
    let wide = sock.dir.join("wide");
    std::fs::create_dir(&wide).unwrap();
    std::fs::set_permissions(&wide, std::fs::Permissions::from_mode(0o755)).unwrap();
    let err = Client::connect_unix(wide.join("s.sock"), Some(TOKEN.into())).unwrap_err();
    assert!(matches!(err, Error::InsecureSocket { .. }), "{err:?}");
    let text = format!("{err:?} {err}");
    assert!(!text.contains(TOKEN));
    assert!(text.contains("0700") && text.contains("0755"), "{text}");
    assert!(!err.is_transient(), "a refusal is never retried");
}

#[test]
fn a_drip_fed_answer_hits_the_body_deadline_and_is_not_retried() {
    let (sock, hits) = server(2, |mut s, _| {
        s.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 10000\r\n\r\n")
            .unwrap();
        for _ in 0..10_000 {
            if s.write_all(b"x").is_err() {
                return;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    });
    let started = Instant::now();
    let err = client(&sock)
        .with_body_deadline(Duration::from_millis(400))
        .unwrap()
        .health()
        .unwrap_err();
    assert!(
        matches!(err, Error::Io(ref e) if e.kind() == std::io::ErrorKind::TimedOut),
        "{err:?}"
    );
    assert!(!err.is_transient());
    assert!(started.elapsed() < Duration::from_secs(5));
    assert_eq!(hits.load(Ordering::SeqCst), 1);
}

#[test]
fn a_request_whose_bytes_were_sent_is_never_replayed() {
    // The server reads each request and closes without answering: a dropped
    // connection is transient for the caller to judge, but the client makes
    // ONE attempt per call (a prompt may already have been acted on).
    let (sock, hits) = server(3, |s, _| drop(s));
    let err = client(&sock)
        .prompt("s-1", &PromptRequest::text("hi"))
        .unwrap_err();
    assert!(err.is_transient(), "{err:?}");
    std::thread::sleep(Duration::from_millis(200));
    assert_eq!(hits.load(Ordering::SeqCst), 1, "one attempt, no replay");
}

#[test]
fn a_per_read_timeout_ends_a_silent_server_and_is_not_transient() {
    let (sock, _) = server(1, |s, _| {
        std::thread::sleep(Duration::from_millis(1500));
        drop(s);
    });
    let started = Instant::now();
    let err = client(&sock)
        .with_request_timeout(Duration::from_millis(150))
        .unwrap()
        .health()
        .unwrap_err();
    assert!(
        matches!(err, Error::Io(ref e) if e.kind() == std::io::ErrorKind::TimedOut),
        "{err:?}"
    );
    assert!(!err.is_transient());
    assert!(started.elapsed() < Duration::from_secs(1));
}

#[test]
fn a_server_that_is_not_listening_is_a_transient_connect_error() {
    let (sock, _) = server(0, |_, _| {});
    // The listener was dropped when the server thread ended (0 connections):
    // the socket file is left, nobody answers.
    std::thread::sleep(Duration::from_millis(100));
    let err = client(&sock).health().unwrap_err();
    assert!(matches!(err, Error::Connect(_)), "{err:?}");
    assert!(err.is_transient());
}

/// A peer that closed the connection before the client set a timeout on it:
/// macOS refuses that `setsockopt` with EINVAL (#1174). It is a dropped
/// connection (transient), never a bad setting (`InvalidConfig`, not retried).
#[test]
fn a_peer_that_closed_first_is_a_dropped_connection_not_a_bad_setting() {
    use tole_client::{Connection, Transport, UnixTransport};
    let (sock, _) = server(0, |_, _| {});
    // `server(0, ..)` binds and then drops its listener; bind our own here.
    let _ = std::fs::remove_file(&sock.path);
    let listener = UnixListener::bind(&sock.path).unwrap();
    let transport = UnixTransport::new(&sock.path).unwrap();
    let mut conn: Box<dyn Connection> = transport.connect().unwrap();
    let (peer, _) = listener.accept().unwrap();
    drop(peer);
    std::thread::sleep(Duration::from_millis(100));
    for result in [
        conn.set_write_timeout(Some(Duration::from_secs(5))),
        conn.set_read_timeout(Some(Duration::from_secs(5))),
    ] {
        // Accepted where the OS allows it, else a dropped connection.
        if let Err(e) = result {
            assert_eq!(e.kind(), std::io::ErrorKind::ConnectionReset, "{e:?}");
            assert!(Error::from(e).is_transient());
        }
    }
}

/// The standard library's refusal of a zero timeout is still a bad setting.
#[test]
fn a_zero_timeout_is_still_a_bad_setting() {
    use tole_client::{Transport, UnixTransport};
    let (sock, _) = server(0, |_, _| {});
    let _ = std::fs::remove_file(&sock.path);
    let _listener = UnixListener::bind(&sock.path).unwrap();
    let mut conn = UnixTransport::new(&sock.path).unwrap().connect().unwrap();
    let err = conn.set_read_timeout(Some(Duration::ZERO)).unwrap_err();
    assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput);
    assert!(matches!(Error::from(err), Error::InvalidConfig(_)));
}
