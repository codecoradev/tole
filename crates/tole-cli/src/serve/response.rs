//! Writing one JSON response: the status line, the headers and the body.

use serde_json::json;
use std::io::Write;
use tole_cli::serve_support::transport::Conn;

pub(super) fn reason_phrase(status: u16) -> &'static str {
    match status {
        200 => "OK",
        202 => "Accepted",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        409 => "Conflict",
        413 => "Payload Too Large",
        415 => "Unsupported Media Type",
        429 => "Too Many Requests",
        500 => "Internal Server Error",
        503 => "Service Unavailable",
        507 => "Insufficient Storage",
        _ => "Unknown",
    }
}

pub(super) fn respond(stream: &mut Conn, status: u16, payload: &serde_json::Value) {
    respond_with(stream, status, payload, "");
}

/// The 401 of every refused request: one body whatever was wrong (no user
/// or password verdict), and the Basic challenge when Basic is on.
pub(super) fn respond_unauthorized(stream: &mut Conn, auth: &tole_cli::auth::Authenticator) {
    let extra = auth
        .challenge()
        .map(|c| format!("WWW-Authenticate: {c}\r\n"))
        .unwrap_or_default();
    respond_with(stream, 401, &json!({"error": "unauthorized"}), &extra);
}

/// `extra` is zero or more complete `Name: value\r\n` header lines.
pub(super) fn respond_with(
    stream: &mut Conn,
    status: u16,
    payload: &serde_json::Value,
    extra: &str,
) {
    // #1314: the error text of a 500 can carry a storage or provider message.
    let mut redacted;
    let mut payload = payload;
    if let (500, Some(e)) = (status, payload["error"].as_str()) {
        redacted = payload.clone();
        redacted["error"] = json!(tole_cli::session::problem::egress_error(e));
        payload = &redacted;
    }
    let body = payload.to_string();
    let reason = reason_phrase(status);
    let head = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\n{extra}Content-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    let _ = stream.write_all(head.as_bytes());
    let _ = stream.write_all(body.as_bytes());
    let _ = stream.flush();
}
