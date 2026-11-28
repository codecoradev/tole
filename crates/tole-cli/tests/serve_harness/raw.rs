//! A request with any `Authorization` value and the whole answer as text, and device tokens
//! made in the data dir of a `Serve` (#1320).

use super::{home_of, Serve};
use serde_json::Value;
use std::io::{Read, Write};
use std::time::Duration;
use tole_cli::serve_support::device_tokens::{Scope, Store};

/// The status line, headers and body of the answer to one request, exactly as sent.
/// `authorization` is the header VALUE (`Bearer ...`); a body is sent as given.
pub fn raw(port: u16, method: &str, path: &str, authorization: Option<&str>, body: &str) -> String {
    let mut s = std::net::TcpStream::connect(("127.0.0.1", port)).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(30))).unwrap();
    let auth = authorization.map_or(String::new(), |a| format!("Authorization: {a}\r\n"));
    let req = format!(
        "{method} {path} HTTP/1.1\r\n{auth}Content-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    s.write_all(req.as_bytes()).unwrap();
    let mut out = String::new();
    let _ = s.read_to_string(&mut out);
    out
}

pub fn status_of(answer: &str) -> u16 {
    answer
        .split_whitespace()
        .nth(1)
        .and_then(|c| c.parse().ok())
        .unwrap_or(0)
}

pub fn json_of(answer: &str) -> Value {
    let body = answer.split_once("\r\n\r\n").map_or("", |(_, b)| b);
    serde_json::from_str(body).unwrap_or(Value::Null)
}

/// The token store of `serve` (its data dir is `<home>/tole`).
pub fn store_of(serve: &Serve) -> Store {
    Store::new(&home_of(&serve.dir).join("tole"))
}

/// A new device token of `scope`, the value to send as `Bearer <value>`.
pub fn device_token(serve: &Serve, name: &str, scope: Scope) -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let added = store_of(serve).add(name, scope, now).unwrap();
    added.secret.expose().to_string()
}
