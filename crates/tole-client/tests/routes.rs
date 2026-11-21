//! What only a scripted one-shot server can show about the new routes (a port
//! the OS picks, no serve process): how an id is put in a path, that an image
//! goes up as raw bytes, and how a malformed image answer is read. The calls
//! against a real serve are in `crates/tole-cli/tests/client_contract.rs`.

use serde_json::{json, Value};
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::mpsc;
use std::time::Duration;
use tole_client::{Client, Error};

/// What the server read: the head (request line and headers) and the body.
struct Seen {
    head: String,
    body: Vec<u8>,
}

/// One connection, answered with `reply` (the whole response), after the
/// request was read in full (its `Content-Length` bytes of body included).
fn server(reply: Vec<u8>) -> (String, mpsc::Receiver<Seen>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let (mut s, _) = listener.accept().unwrap();
        s.set_read_timeout(Some(Duration::from_secs(20))).unwrap();
        let seen = read_request(&mut s);
        let _ = tx.send(seen);
        let _ = s.write_all(&reply);
    });
    (url, rx)
}

fn read_request(s: &mut TcpStream) -> Seen {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 4096];
    let split = loop {
        if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break i + 4;
        }
        let n = s.read(&mut chunk).unwrap();
        assert!(n > 0, "the request ended inside its head");
        buf.extend_from_slice(&chunk[..n]);
    };
    let head = String::from_utf8_lossy(&buf[..split]).into_owned();
    let want = head
        .lines()
        .find_map(|l| {
            let (k, v) = l.split_once(':')?;
            k.eq_ignore_ascii_case("content-length").then(|| v.trim())
        })
        .map_or(0, |v| v.parse::<usize>().unwrap());
    while buf.len() < split + want {
        let n = s.read(&mut chunk).unwrap();
        assert!(n > 0, "the request ended inside its body");
        buf.extend_from_slice(&chunk[..n]);
    }
    Seen {
        head,
        body: buf[split..split + want].to_vec(),
    }
}

fn json_reply(status: &str, body: &Value) -> Vec<u8> {
    let body = body.to_string();
    format!(
        "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
    .into_bytes()
}

fn client(url: &str) -> Client {
    Client::connect_tcp(url, Some("tok".into()), false).unwrap()
}

fn request_line(seen: &Seen) -> &str {
    seen.head.lines().next().unwrap()
}

/// An id is one path segment whatever it contains: it cannot add a segment, a
/// query or a header line.
#[test]
fn an_id_never_adds_a_segment_to_the_path() {
    let (url, rx) = server(json_reply(
        "404 Not Found",
        &json!({"error": "unknown session"}),
    ));
    let err = client(&url).detach("../x?y=1\r\nHost: z").unwrap_err();
    assert!(matches!(err, Error::Status { status: 404, .. }), "{err:?}");
    let seen = rx.recv_timeout(Duration::from_secs(20)).unwrap();
    assert_eq!(
        request_line(&seen),
        "POST /sessions/..%2Fx%3Fy%3D1%0D%0AHost%3A%20z/detach HTTP/1.1"
    );
    assert_eq!(seen.head.matches("Host:").count(), 1, "{}", seen.head);
}

const PNG: &[u8] = &[0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a, 1, 2, 3];

/// The upload sends the raw bytes (not JSON) and says so; the answer is the
/// reference.
#[test]
fn an_image_goes_up_as_raw_bytes() {
    let (url, rx) = server(json_reply(
        "200 OK",
        &json!({"id": "0123456789abcdef", "mime": "image/png", "bytes": 11, "fnv1a64": "0123456789abcdef"}),
    ));
    let up = client(&url).upload_image("s1", PNG).unwrap();
    assert_eq!((up.mime.as_str(), up.bytes), ("image/png", 11));
    let seen = rx.recv_timeout(Duration::from_secs(20)).unwrap();
    assert_eq!(request_line(&seen), "POST /sessions/s1/images HTTP/1.1");
    assert!(
        seen.head
            .contains("Content-Type: application/octet-stream\r\n"),
        "{}",
        seen.head
    );
    assert_eq!(seen.body, PNG);
}

/// A body over the per-image cap is refused before anything is sent.
#[test]
fn an_oversized_image_is_refused_before_a_connection_is_opened() {
    // Nothing listens here: a connection attempt would be `Error::Connect`.
    let c = Client::connect_tcp("http://127.0.0.1:1", None, false).unwrap();
    let big = vec![0u8; tole_core::image::MAX_IMAGE_BYTES as usize + 1];
    let err = c.upload_image("s1", &big).unwrap_err();
    assert!(matches!(err, Error::InvalidConfig(_)), "{err:?}");
}

fn image_reply(content_type: Option<&str>, body: &[u8]) -> Vec<u8> {
    let ct = content_type
        .map(|t| format!("Content-Type: {t}\r\n"))
        .unwrap_or_default();
    let mut out = format!(
        "HTTP/1.1 200 OK\r\n{ct}Content-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    )
    .into_bytes();
    out.extend_from_slice(body);
    out
}

/// The image comes back as bytes with the type the server recorded, binary
/// bytes intact; the hash is one path segment.
#[test]
fn an_image_is_read_back_with_its_recorded_type() {
    let (url, rx) = server(image_reply(Some("Image/PNG; charset=binary"), PNG));
    let img = client(&url).image("s1", "0123456789abcdef").unwrap();
    assert_eq!((img.mime.as_str(), img.data.as_slice()), ("image/png", PNG));
    let seen = rx.recv_timeout(Duration::from_secs(20)).unwrap();
    assert_eq!(
        request_line(&seen),
        "GET /sessions/s1/images/0123456789abcdef HTTP/1.1"
    );
    assert!(seen.head.contains("Accept: image/*"), "{}", seen.head);

    let (url, rx) = server(image_reply(Some("image/png"), PNG));
    let _ = client(&url).image("s1", "../../x");
    let seen = rx.recv_timeout(Duration::from_secs(20)).unwrap();
    assert_eq!(
        request_line(&seen),
        "GET /sessions/s1/images/..%2F..%2Fx HTTP/1.1"
    );
}

#[test]
fn an_image_answer_without_a_type_or_over_the_cap_is_an_error() {
    let (url, _rx) = server(image_reply(None, PNG));
    let err = client(&url).image("s1", "0123456789abcdef").unwrap_err();
    assert!(matches!(err, Error::Protocol(_)), "{err:?}");

    let big = vec![7u8; tole_core::image::MAX_IMAGE_BYTES as usize + 1];
    let (url, _rx) = server(image_reply(Some("image/png"), &big));
    let err = client(&url).image("s1", "0123456789abcdef").unwrap_err();
    assert!(matches!(err, Error::Protocol(_)), "{err:?}");
}

/// A refusal of the image read is the usual JSON error, not bytes.
#[test]
fn an_image_that_is_not_recorded_is_a_status_error() {
    let (url, _rx) = server(json_reply(
        "404 Not Found",
        &json!({"error": "no such image in this session"}),
    ));
    match client(&url).image("s1", "0123456789abcdef").unwrap_err() {
        Error::Status {
            status, message, ..
        } => {
            assert_eq!(status, 404);
            assert_eq!(message, "no such image in this session");
        }
        e => panic!("{e:?}"),
    }
}
