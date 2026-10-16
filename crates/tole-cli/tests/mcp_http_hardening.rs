//! Hardening of `tole serve --transport mcp` over the REAL binary, at parity
//! with the REST transport: a failed-auth limiter per source IP, a request
//! body ceiling (413, declared or streamed) and a working happy path.

// Spawns the real `tole serve --transport mcp`, which needs both features.
#![cfg(all(feature = "shell-tools", feature = "mcp-http"))]

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::process::{Child, Command, Stdio};
use std::time::Duration;

const TOKEN: &str = "t0ken";
const INIT: &str = r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"t","version":"1"}}}"#;

struct Server {
    child: Child,
    port: u16,
}

impl Server {
    fn spawn() -> Self {
        let port = TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let child = Command::new(env!("CARGO_BIN_EXE_tole"))
            .args([
                "serve",
                "--transport",
                "mcp",
                "--port",
                &port.to_string(),
                "--token",
                TOKEN,
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn tole serve --transport mcp");
        let me = Self { child, port };
        // Ready = it answers TCP. (A wrong-token probe would burn the
        // limiter budget the tests below measure.)
        for _ in 0..100 {
            if TcpStream::connect(("127.0.0.1", port)).is_ok() {
                return me;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        panic!("mcp server never listened");
    }

    fn connect(&self) -> TcpStream {
        let s = TcpStream::connect(("127.0.0.1", self.port)).unwrap();
        s.set_read_timeout(Some(Duration::from_secs(15))).unwrap();
        s
    }

    /// POST `body` with `auth` (the whole header value, or none) and return
    /// the status code.
    fn post(&self, auth: Option<&str>, body: &str) -> u16 {
        let mut s = self.connect();
        let auth = auth.map_or(String::new(), |a| format!("Authorization: {a}\r\n"));
        let req = format!(
            "POST /mcp HTTP/1.1\r\nHost: 127.0.0.1:{}\r\n{auth}Content-Type: application/json\r\nAccept: application/json, text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            self.port,
            body.len()
        );
        s.write_all(req.as_bytes()).unwrap();
        status_of(&mut s)
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Read until the status line is complete and parse the code.
fn status_of(s: &mut TcpStream) -> u16 {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 4096];
    loop {
        match s.read(&mut chunk) {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                buf.extend_from_slice(&chunk[..n]);
                if buf.windows(2).any(|w| w == b"\r\n") {
                    break;
                }
            }
        }
    }
    let line = String::from_utf8_lossy(&buf);
    line.split_whitespace()
        .nth(1)
        .and_then(|c| c.parse().ok())
        .unwrap_or_else(|| panic!("no status line in {line:?}"))
}

#[test]
fn a_valid_request_still_works_and_the_scheme_is_case_insensitive() {
    let srv = Server::spawn();
    assert_eq!(srv.post(Some("Bearer t0ken"), INIT), 200);
    assert_eq!(srv.post(Some("bearer t0ken"), INIT), 200);
}

#[test]
fn wrong_tokens_are_401_then_the_ip_is_limited_even_with_the_right_token() {
    let srv = Server::spawn();
    assert_eq!(srv.post(None, INIT), 401, "missing header");
    for i in 1..10 {
        assert_eq!(
            srv.post(Some("Bearer nope"), INIT),
            401,
            "failure #{}",
            i + 1
        );
    }
    // The tenth failure used the budget up: from now on 429, and the right
    // token does not get through until the window rolls over (REST parity).
    assert_eq!(srv.post(Some("Bearer nope"), INIT), 429);
    assert_eq!(srv.post(Some("Bearer t0ken"), INIT), 429);
}

#[test]
fn a_declared_oversized_body_is_413_before_it_is_read() {
    let srv = Server::spawn();
    let mut s = srv.connect();
    // Headers only: the server must answer from Content-Length alone.
    let req = format!(
        "POST /mcp HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nAuthorization: Bearer {TOKEN}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        srv.port,
        2 * 1024 * 1024
    );
    s.write_all(req.as_bytes()).unwrap();
    assert_eq!(status_of(&mut s), 413);
}

#[test]
fn a_chunked_oversized_body_is_413_while_it_streams() {
    let srv = Server::spawn();
    let mut s = srv.connect();
    let head = format!(
        "POST /mcp HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nAuthorization: Bearer {TOKEN}\r\nContent-Type: application/json\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n",
        srv.port
    );
    s.write_all(head.as_bytes()).unwrap();
    let block = vec![b'x'; 64 * 1024];
    // 3 MiB in 64 KiB chunks; the server stops reading at the ceiling, so a
    // later write may fail with a reset: that is fine, the answer is what
    // counts.
    for _ in 0..48 {
        let chunk = format!("{:x}\r\n", block.len());
        if s.write_all(chunk.as_bytes()).is_err()
            || s.write_all(&block).is_err()
            || s.write_all(b"\r\n").is_err()
        {
            break;
        }
    }
    let _ = s.write_all(b"0\r\n\r\n");
    assert_eq!(status_of(&mut s), 413);
}
