//! Tests of the connection layer: the token, the status line and the read deadline.

use crate::serve::request::DeadlineReader;
use crate::serve::*;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::time::{Duration, Instant};
use tole_cli::serve_support::transport::Conn;

/// #682: the token comes from a file first, then the flag, then the
/// environment; an empty file is an error, and no error shows a token.
#[test]
fn the_serve_token_prefers_the_file_and_never_echoes_a_value() {
    let dir = crate::test_dir::TestDir::new("tole-serve-token");
    let file = dir.join("token");
    std::fs::write(&file, "file-token-0682\n").unwrap();
    let s = |v: &str| Some(v.to_string());
    assert_eq!(
        serve_token(None, Some(&file), s("env-token")).unwrap(),
        "file-token-0682"
    );
    assert_eq!(
        tole_core::secrets::process_secrets().redact("t=file-token-0682"),
        "t=«TOLE_SERVE_TOKEN»",
        "the token is a secret held by role"
    );
    assert_eq!(
        serve_token(s(" flag-token "), None, s("env-token")).unwrap(),
        "flag-token"
    );
    assert_eq!(
        serve_token(s("  "), None, s("env-token")).unwrap(),
        "env-token"
    );
    let empty = dir.join("empty");
    std::fs::write(&empty, "\n").unwrap();
    let e = serve_token(None, Some(&empty), s("env-token-0682")).unwrap_err();
    assert!(e.contains("is empty") && !e.contains("env-token"), "{e}");
    let e = serve_token(None, Some(&dir.join("missing")), None).unwrap_err();
    assert!(e.contains("does not exist"), "{e}");
    let e = serve_token(None, None, s(" ")).unwrap_err();
    assert!(e.contains("TOLE_SERVE_TOKEN"), "{e}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn reason_phrases_cover_every_status_the_routes_emit() {
    assert_eq!(reason_phrase(202), "Accepted");
    assert_eq!(reason_phrase(429), "Too Many Requests");
    assert_eq!(reason_phrase(503), "Service Unavailable");
    assert_eq!(reason_phrase(413), "Payload Too Large");
    assert_eq!(reason_phrase(403), "Forbidden");
}

#[test]
fn deadline_reader_bounds_a_trickling_client() {
    // A client sending one byte per 100 ms never trips the per-read
    // timeout; the total deadline must still cut the read off.
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let writer = std::thread::spawn(move || {
        let mut c = TcpStream::connect(addr).unwrap();
        for _ in 0..40 {
            if c.write_all(b"x").is_err() {
                return;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    });
    let (server_side, _) = listener.accept().unwrap();
    let server_side = Conn::Tcp(server_side);
    let mut r = DeadlineReader {
        stream: &server_side,
        deadline: Instant::now() + Duration::from_millis(400),
    };
    let started = Instant::now();
    let mut buf = [0u8; 1];
    let err = loop {
        match r.read(&mut buf) {
            Ok(0) => panic!("peer closed before the deadline"),
            Ok(_) => continue,
            Err(e) => break e,
        }
    };
    assert!(matches!(
        err.kind(),
        std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock
    ));
    assert!(started.elapsed() < Duration::from_secs(2));
    drop(server_side);
    let _ = writer.join();
}
