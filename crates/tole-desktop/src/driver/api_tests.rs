//! `ClientApi` forwards each call of the task driver to its route, with the
//! right method, path and body, and decodes the answer.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Mutex};

use tole_client::{Client, Decision, InterruptRequest, PromptRequest};

use super::api::{Api, ClientApi};

/// A serve that answers every request with canned JSON by path, and records
/// `METHOD target body` of each.
fn serve(seen: Arc<Mutex<Vec<String>>>) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let url = format!("http://{}", listener.local_addr().expect("addr"));
    std::thread::spawn(move || {
        for conn in listener.incoming() {
            let Ok(conn) = conn else { return };
            answer(conn, &seen);
        }
    });
    url
}

fn answer(mut conn: TcpStream, seen: &Mutex<Vec<String>>) {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 4096];
    let (head, body_at) = loop {
        let n = conn.read(&mut chunk).unwrap_or(0);
        buf.extend_from_slice(&chunk[..n]);
        let text = String::from_utf8_lossy(&buf).into_owned();
        if let Some(i) = text.find("\r\n\r\n") {
            break (text[..i].to_owned(), i + 4);
        }
        if n == 0 {
            return;
        }
    };
    let length = head
        .lines()
        .find_map(|l| {
            l.to_ascii_lowercase()
                .strip_prefix("content-length:")
                .map(str::to_owned)
        })
        .and_then(|v| v.trim().parse::<usize>().ok())
        .unwrap_or(0);
    while buf.len() < body_at + length {
        let n = conn.read(&mut chunk).unwrap_or(0);
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&chunk[..n]);
    }
    let body = String::from_utf8_lossy(&buf[body_at..]).into_owned();
    let mut first = head.lines().next().unwrap_or("").split(' ');
    let (method, target) = (first.next().unwrap_or(""), first.next().unwrap_or(""));
    seen.lock()
        .unwrap()
        .push(format!("{method} {target} {body}"));
    let reply = match target.split('?').next().unwrap_or("") {
        "/sessions/s1/entries" => {
            r#"{"id":"s1","entries":[],"nextAfter":7,"hasMore":false,"lastSeq":7}"#
        }
        "/sessions/s1/status" => {
            r#"{"id":"s1","entries":null,"busy":true,"usage_report":null,"model":null,"effort":null,"contextWindow":null,"contextUsed":null,"name":null,"snapshotAt":null,"stale":false}"#
        }
        "/sessions/s1/prompt" => r#"{"stopReason":"end_turn"}"#,
        "/sessions/s1/cancel" => r#"{"cancelled":true,"sessionId":"s1"}"#,
        "/sessions/s1/interrupt" => r#"{"queued":true,"urgent":false,"sessionId":"s1"}"#,
        "/approvals/a1/decision" => r#"{"id":"a1","status":"approved"}"#,
        "/approvals" => r#"{"approvals":[]}"#,
        _ => r#"{"error":"no such route"}"#,
    };
    let ok = !reply.contains("no such route");
    let status = if ok { "200 OK" } else { "404 Not Found" };
    let out = format!(
        "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{reply}",
        reply.len()
    );
    let _ = conn.write_all(out.as_bytes());
}

#[test]
fn every_call_of_the_task_driver_reaches_its_route_and_is_decoded() {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let client = Client::connect_tcp(&serve(Arc::clone(&seen)), None, false).expect("client");
    let api = ClientApi::new(client).expect("bounded");

    assert_eq!(
        api.entries("s1", 3, Some(500)).expect("entries").last_seq,
        7
    );
    assert!(api.status("s1").expect("status").busy);
    let reply = api.prompt(
        "s1",
        &PromptRequest {
            text: "hi".into(),
            images: vec![],
        },
    );
    assert_eq!(reply.expect("prompt").stop_reason, "end_turn");
    assert!(api.cancel("s1").expect("cancel").cancelled);
    let note = api.interrupt("s1", &InterruptRequest::text("also"));
    assert!(note.expect("interrupt").queued);
    let decided = api.decide("a1", Decision::Allow).expect("decide");
    assert_eq!(
        (decided.id.as_str(), decided.status.as_str()),
        ("a1", "approved")
    );
    assert!(api
        .approvals(Some("s1"))
        .expect("approvals")
        .approvals
        .is_empty());
    assert!(
        api.status("nope").is_err(),
        "an unknown session is a refusal, not a panic"
    );

    let seen = seen.lock().unwrap().clone();
    let wanted = [
        "GET /sessions/s1/entries?after=3&limit=500 ",
        "GET /sessions/s1/status ",
        "POST /sessions/s1/prompt {\"text\":\"hi\"}",
        "POST /sessions/s1/cancel ",
        "POST /sessions/s1/interrupt {\"text\":\"also\"}",
        "POST /approvals/a1/decision {\"decision\":\"allow\"}",
        "GET /approvals?session=s1 ",
    ];
    for (got, want) in seen.iter().zip(wanted) {
        assert_eq!(got, want);
    }
    assert_eq!(seen.len(), wanted.len() + 1);
}
