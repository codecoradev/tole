//! A call cannot be held forever by a server that answers and then drips, in
//! any framing, and unusable settings are refused up front.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tole_client::{Client, Decision, Error, PromptRequest};

/// Serve one connection with `script`, which gets the socket after the
/// request was read.
fn server(script: impl FnOnce(TcpStream) + Send + 'static) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    std::thread::spawn(move || {
        let (mut s, _) = listener.accept().unwrap();
        let mut head = [0u8; 1024];
        let _ = s.read(&mut head);
        script(s);
    });
    url
}

/// One byte every 50 ms, `n` times: every socket read succeeds, so only an
/// overall deadline can stop the call.
fn drip(s: &mut TcpStream, bytes: &[u8], n: usize) {
    for _ in 0..n {
        for b in bytes {
            if s.write_all(&[*b]).is_err() {
                return;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }
}

fn client(url: &str) -> Client {
    Client::connect_tcp(url, None, false)
        .unwrap()
        .with_body_deadline(Duration::from_millis(400))
        .unwrap()
}

fn assert_times_out(url: &str) {
    let started = Instant::now();
    let err = client(url).health().unwrap_err();
    // The same kind on every platform (Unix reports a socket timeout as
    // WouldBlock; the client turns it into TimedOut), and not retryable.
    assert!(
        matches!(err, Error::Io(ref e) if e.kind() == std::io::ErrorKind::TimedOut),
        "{err:?}"
    );
    assert!(
        !err.is_transient(),
        "a response that timed out is not replayed"
    );
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "{:?}",
        started.elapsed()
    );
}

#[test]
fn a_drip_fed_content_length_body_hits_the_deadline() {
    let url = server(|mut s| {
        s.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 10000\r\n\r\n")
            .unwrap();
        drip(&mut s, b"x", 10_000);
    });
    assert_times_out(&url);
}

#[test]
fn a_drip_fed_chunk_size_line_hits_the_deadline() {
    // One chunk-size line (with an extension) that never ends: a single
    // `read` of the body makes many socket reads here.
    let url = server(|mut s| {
        s.write_all(b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n")
            .unwrap();
        drip(&mut s, b"1;ext=", 1);
        drip(&mut s, b"x", 10_000);
    });
    assert_times_out(&url);
}

#[test]
fn drip_fed_trailers_hit_the_deadline() {
    let url = server(|mut s| {
        s.write_all(b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n0\r\n")
            .unwrap();
        // Trailer lines, each completed slowly, one byte at a time.
        drip(&mut s, b"X-Trailer: y\r\n", 100);
    });
    assert_times_out(&url);
}

#[test]
fn drip_fed_headers_hit_the_deadline() {
    // The status line is in; the rest of the head never finishes.
    let url = server(|mut s| {
        s.write_all(b"HTTP/1.1 200 OK\r\n").unwrap();
        drip(&mut s, b"X-Slow: y\r\n", 100);
    });
    assert_times_out(&url);
}

#[test]
fn a_huge_error_body_is_cut_in_the_message() {
    // 3 MiB of text, and a JSON body whose `error` is 3 MiB: neither makes the
    // message of the error longer than a few KiB.
    let big = "x".repeat(3 * 1024 * 1024);
    let json = format!("{{\"error\":\"{big}\"}}");
    for body in [big.clone(), json] {
        let url = server(move |mut s| {
            let head = format!(
                "HTTP/1.1 500 Oops\r\nContent-Length: {}\r\n\r\n",
                body.len()
            );
            s.write_all(head.as_bytes()).unwrap();
            s.write_all(body.as_bytes()).unwrap();
        });
        let err = Client::connect_tcp(&url, None, false)
            .unwrap()
            .health()
            .unwrap_err();
        let Error::Status {
            status, message, ..
        } = &err
        else {
            panic!("{err:?}")
        };
        assert_eq!(*status, 500);
        assert_eq!(
            message.len(),
            tole_client::MAX_STATUS_MESSAGE + '…'.len_utf8()
        );
        assert!(message.ends_with('…'));
        assert!(err.to_string().len() < 5000);
    }
}

#[test]
fn an_error_body_over_the_cap_is_not_parsed_and_a_success_body_keeps_its_limit() {
    // 3 MiB of `1,`: 2 bytes each on the wire, a 32-byte `Value` each once
    // parsed. As an error body it is cut at MAX_ERROR_BODY and never parsed.
    let array = format!("[{}1]", "1,".repeat(3 * 512 * 1024));
    let url = server(move |mut s| {
        let head = format!(
            "HTTP/1.1 502 Bad Gateway\r\nContent-Length: {}\r\n\r\n",
            array.len()
        );
        s.write_all(head.as_bytes()).unwrap();
        // The client stops reading at the cap: the rest may meet a closed socket.
        let _ = s.write_all(array.as_bytes());
    });
    let err = Client::connect_tcp(&url, None, false)
        .unwrap()
        .health()
        .unwrap_err();
    let Error::Status {
        status,
        message,
        body,
    } = &err
    else {
        panic!("{err:?}")
    };
    assert_eq!(*status, 502);
    assert!(body.is_null(), "an over-cap error body is not parsed");
    assert!(
        message.starts_with("[1,1,1") && message.ends_with('…'),
        "{}",
        message.len()
    );
    assert!(err.is_transient(), "the status still classifies the error");
    // A small error body is parsed as before.
    let url = server(|mut s| {
        let body = r#"{"error":"busy","n":1}"#;
        let head = format!(
            "HTTP/1.1 409 Conflict\r\nContent-Length: {}\r\n\r\n",
            body.len()
        );
        s.write_all(head.as_bytes()).unwrap();
        s.write_all(body.as_bytes()).unwrap();
    });
    let err = Client::connect_tcp(&url, None, false)
        .unwrap()
        .health()
        .unwrap_err();
    assert!(
        matches!(&err, Error::Status { message, body, .. }
        if message == "busy" && body["n"] == 1),
        "{err:?}"
    );
    // A success body is not held to the error cap: 2 MiB is fine.
    let url = server(|mut s| {
        let body = format!(
            r#"{{"status":"ok","version":"{}"}}"#,
            "v".repeat(2 * 1024 * 1024)
        );
        let head = format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n", body.len());
        s.write_all(head.as_bytes()).unwrap();
        s.write_all(body.as_bytes()).unwrap();
    });
    let health = Client::connect_tcp(&url, None, false)
        .unwrap()
        .health()
        .unwrap();
    assert_eq!(health.version.map(|v| v.len()), Some(2 * 1024 * 1024));
}

#[test]
fn unusable_settings_are_refused_up_front_and_a_huge_deadline_does_not_panic() {
    let c = Client::connect_tcp("http://127.0.0.1:1", None, false).unwrap();
    for refused in [
        c.clone().with_request_timeout(Duration::ZERO).unwrap_err(),
        c.clone().with_body_deadline(Duration::ZERO).unwrap_err(),
    ] {
        assert!(matches!(refused, Error::InvalidConfig(_)), "{refused:?}");
        assert!(!refused.is_transient(), "a bad setting is not retried");
    }
    // `Duration::MAX` means "no deadline" and must not overflow `Instant`.
    let url = server(|mut s| {
        s.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 15\r\n\r\n{\"status\":\"ok\"}")
            .unwrap();
    });
    let c = Client::connect_tcp(&url, None, false)
        .unwrap()
        .with_body_deadline(Duration::MAX)
        .unwrap();
    assert_eq!(c.health().unwrap().status, "ok");
}

#[test]
fn a_request_whose_bytes_were_sent_is_never_replayed() {
    // The server reads each request and closes without answering. A POST
    // (prompt, approval decision, cancel) may already have been acted on, so
    // the client must make ONE attempt per call and leave the choice to the
    // caller.
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let hits = Arc::new(AtomicUsize::new(0));
    let seen = Arc::clone(&hits);
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            seen.fetch_add(1, Ordering::SeqCst);
            let mut s = stream;
            let mut buf = [0u8; 4096];
            let _ = s.read(&mut buf);
            // dropped: no response
        }
    });
    let c = Client::connect_tcp(&url, None, false).unwrap();
    assert!(c.prompt("s", &PromptRequest::text("hi")).is_err());
    assert!(c.decide("apr-1", Decision::Allow).is_err());
    assert!(c.cancel("s").is_err());
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(
        hits.load(Ordering::SeqCst),
        3,
        "one connection per call, no retry"
    );
}

#[test]
fn a_peer_that_stops_reading_cannot_block_the_request_write() {
    // The server accepts and never reads; the body is far larger than the
    // socket buffers, so the write itself would block forever.
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    std::thread::spawn(move || {
        let (s, _) = listener.accept().unwrap();
        std::thread::sleep(Duration::from_secs(20));
        drop(s);
    });
    let c = client(&url);
    let started = Instant::now();
    let err = c
        .prompt("s", &PromptRequest::text("x".repeat(48 * 1024 * 1024)))
        .unwrap_err();
    assert!(
        matches!(err, Error::Io(ref e) if e.kind() == std::io::ErrorKind::TimedOut),
        "{err:?}"
    );
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "{:?}",
        started.elapsed()
    );
}
