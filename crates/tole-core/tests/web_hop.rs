//! #625: `web::vetted_get`, the single hop the built-in web tools are built
//! on. Local servers on 127.0.0.1 only (the private-address opt-out is
//! passed explicitly, never through the environment).

use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tole_core::web::{vet_public_url, vetted_get, Hop, HopError};

type Seen = Arc<Mutex<Vec<String>>>;

/// Serves one canned raw response per connection, recording each request
/// head; `delay` holds the answer back.
fn serve(responses: Vec<String>, delay: Duration) -> (String, Seen) {
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = l.local_addr().unwrap();
    let seen: Seen = Arc::default();
    let s2 = Arc::clone(&seen);
    std::thread::spawn(move || {
        for raw in responses {
            let Ok((mut s, _)) = l.accept() else { return };
            let mut r = BufReader::new(s.try_clone().unwrap());
            let mut head = String::new();
            loop {
                let mut line = String::new();
                if r.read_line(&mut line).unwrap_or(0) == 0 || line.trim().is_empty() {
                    break;
                }
                head.push_str(&line);
            }
            s2.lock().unwrap().push(head);
            std::thread::sleep(delay);
            let _ = s.write_all(raw.as_bytes());
        }
    });
    (format!("http://{addr}"), seen)
}

fn ok(status: &str, extra: &str, body: &str) -> String {
    format!(
        "HTTP/1.1 {status}\r\nContent-Type: text/html\r\n{extra}Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
}

fn hop<'a>(url: &'a str, headers: &'a [(String, String)]) -> Hop<'a> {
    Hop {
        url,
        headers,
        timeout: Duration::from_secs(5),
        max_bytes: 1024,
        allow_private: true,
        truncate: false,
    }
}

#[test]
fn a_hop_returns_status_headers_and_body_and_sends_the_headers() {
    let (base, seen) = serve(
        vec![
            ok("200 OK", "X-Thing: Yes\r\n", "<p>hi</p>"),
            ok("404 Not Found", "", "gone"),
        ],
        Duration::ZERO,
    );
    let headers = vec![("User-Agent".to_string(), "tole-test/1".to_string())];
    let url = format!("{base}/page");
    let r = vetted_get(&hop(&url, &headers)).unwrap();
    assert_eq!(
        (r.status, r.body.as_slice(), r.truncated),
        (200, &b"<p>hi</p>"[..], false)
    );
    assert!(
        r.headers.contains(&("x-thing".into(), "Yes".into())),
        "{:?}",
        r.headers
    );
    assert!(seen.lock().unwrap()[0].contains("tole-test/1"));
    // A non-2xx status is a response, not an error (the caller decides).
    let r = vetted_get(&hop(&url, &headers)).unwrap();
    assert_eq!((r.status, r.body.as_slice()), (404, &b"gone"[..]));
}

#[test]
fn a_redirect_is_returned_not_followed_and_its_body_not_read() {
    let big = "x".repeat(10_000);
    let (base, seen) = serve(
        vec![ok("302 Found", "Location: /next\r\n", &big)],
        Duration::ZERO,
    );
    let url = format!("{base}/start");
    let r = vetted_get(&hop(&url, &[])).unwrap();
    assert_eq!(r.status, 302);
    assert!(r.body.is_empty(), "redirect body not read");
    assert!(r
        .headers
        .iter()
        .any(|(k, v)| k == "location" && v == "/next"));
    assert_eq!(seen.lock().unwrap().len(), 1, "no hop followed");
}

#[test]
fn the_body_cap_errors_or_truncates() {
    let (base, _) = serve(
        vec![
            ok("200 OK", "", &"a".repeat(100)),
            ok("200 OK", "", &"a".repeat(100)),
        ],
        Duration::ZERO,
    );
    let url = format!("{base}/");
    let mut h = hop(&url, &[]);
    h.max_bytes = 10;
    assert_eq!(vetted_get(&h), Err(HopError::TooLarge(10)));
    h.truncate = true;
    let r = vetted_get(&h).unwrap();
    assert_eq!((r.body.len(), r.truncated), (10, true));
}

#[test]
fn private_addresses_are_refused_unless_allowed() {
    let (base, seen) = serve(vec![ok("200 OK", "", "secret admin page")], Duration::ZERO);
    let url = format!("{base}/");
    let mut h = hop(&url, &[]);
    h.allow_private = false;
    assert!(matches!(vetted_get(&h), Err(HopError::Private(_))));
    assert!(
        seen.lock().unwrap().is_empty(),
        "nothing reached the server"
    );
    assert!(vet_public_url(&url, false).is_err());
    assert!(vet_public_url(&url, true).is_ok());
    assert!(vet_public_url("file:///etc/passwd", true).is_err());
    assert_eq!(
        vetted_get(&hop("file:///etc/passwd", &[])),
        Err(HopError::InvalidUrl)
    );
}

#[test]
fn the_timeout_holds() {
    let (base, _) = serve(vec![ok("200 OK", "", "late")], Duration::from_secs(3));
    let url = format!("{base}/");
    let mut h = hop(&url, &[]);
    h.timeout = Duration::from_millis(300);
    let started = std::time::Instant::now();
    assert_eq!(vetted_get(&h), Err(HopError::Timeout));
    assert!(started.elapsed() < Duration::from_secs(2));
}
