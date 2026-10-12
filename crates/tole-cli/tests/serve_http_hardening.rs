//! HTTP-framing hardening of `tole serve` over the REAL binary: header
//! names are case-insensitive, `Expect: 100-continue` is answered,
//! oversized and ambiguous bodies get a proper 4xx, and a wrong token is
//! still refused.

// Spawns the real `tole serve` binary, which only exists with the
// `shell-tools` feature (#349).
#![cfg(feature = "shell-tools")]

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

struct Serve {
    child: Child,
    port: u16,
}

impl Serve {
    fn spawn() -> Self {
        let port = TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let child = Command::new(env!("CARGO_BIN_EXE_tole"))
            .args(["serve", "--port", &port.to_string(), "--token", "t0ken"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .spawn()
            .expect("spawn tole serve");
        let me = Self { child, port };
        for _ in 0..50 {
            if let Ok(r) = me.raw("GET /health HTTP/1.1\r\nConnection: close\r\n\r\n") {
                if r.starts_with("HTTP/1.1 200") {
                    return me;
                }
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        panic!("serve never became healthy");
    }

    fn connect(&self) -> std::io::Result<TcpStream> {
        let s = TcpStream::connect(("127.0.0.1", self.port))?;
        s.set_read_timeout(Some(Duration::from_secs(10)))?;
        Ok(s)
    }

    /// Send `req` verbatim, return everything the server answers.
    fn raw(&self, req: &str) -> std::io::Result<String> {
        let mut s = self.connect()?;
        s.write_all(req.as_bytes())?;
        let mut out = String::new();
        let _ = s.read_to_string(&mut out);
        Ok(out)
    }
}

impl Drop for Serve {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn status(resp: &str) -> &str {
    resp.lines().next().unwrap_or("")
}

#[test]
fn authorization_header_is_case_insensitive() {
    let s = Serve::spawn();
    for hdr in [
        "Authorization: Bearer t0ken",
        "authorization: Bearer t0ken",
        "AUTHORIZATION: bearer t0ken",
    ] {
        let r = s
            .raw(&format!(
                "GET /sessions HTTP/1.1\r\n{hdr}\r\nConnection: close\r\n\r\n"
            ))
            .unwrap();
        assert!(status(&r).contains("200"), "{hdr} -> {}", status(&r));
    }
    // The token itself stays case-sensitive and wrong tokens are refused.
    let r = s
        .raw("GET /sessions HTTP/1.1\r\nauthorization: Bearer T0KEN\r\nConnection: close\r\n\r\n")
        .unwrap();
    assert!(status(&r).contains("401"), "{}", status(&r));
}

#[test]
fn expect_100_continue_gets_an_interim_response_before_the_body() {
    let s = Serve::spawn();
    let body = "z".repeat(2048); // not JSON: the route answers 400 once read
    let mut c = s.connect().unwrap();
    c.write_all(
        format!(
            "POST /sessions HTTP/1.1\r\nAuthorization: Bearer t0ken\r\nExpect: 100-continue\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        )
        .as_bytes(),
    )
    .unwrap();
    // Like curl: wait for the interim response BEFORE sending the body.
    let started = Instant::now();
    let mut buf = [0u8; 64];
    let n = c.read(&mut buf).unwrap();
    let interim = String::from_utf8_lossy(&buf[..n]).to_string();
    assert!(interim.starts_with("HTTP/1.1 100 Continue"), "{interim}");
    assert!(
        started.elapsed() < Duration::from_millis(900),
        "interim response took {:?}",
        started.elapsed()
    );
    c.write_all(body.as_bytes()).unwrap();
    let mut rest = String::new();
    let _ = c.read_to_string(&mut rest);
    assert!(rest.starts_with("HTTP/1.1 400"), "{rest}");
}

#[test]
fn oversized_body_is_413_not_a_dropped_connection() {
    let s = Serve::spawn();
    let r = s
        .raw("POST /sessions HTTP/1.1\r\nAuthorization: Bearer t0ken\r\nContent-Length: 2000000\r\nConnection: close\r\n\r\n")
        .unwrap();
    assert!(status(&r).contains("413 Payload Too Large"), "{r}");
}

#[test]
fn ambiguous_framing_is_rejected() {
    let s = Serve::spawn();
    let dup = s
        .raw("POST /sessions HTTP/1.1\r\nAuthorization: Bearer t0ken\r\nContent-Length: 2\r\nContent-Length: 5\r\nConnection: close\r\n\r\n{}")
        .unwrap();
    assert!(status(&dup).contains("400"), "{dup}");
    let te = s
        .raw("POST /sessions HTTP/1.1\r\nAuthorization: Bearer t0ken\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n0\r\n\r\n")
        .unwrap();
    assert!(status(&te).contains("400"), "{te}");
    let bad = s
        .raw("POST /sessions HTTP/1.1\r\nAuthorization: Bearer t0ken\r\nContent-Length: abc\r\nConnection: close\r\n\r\n")
        .unwrap();
    assert!(status(&bad).contains("400"), "{bad}");
}
