//! #1094: the event stream of a discussion against a scripted server: the same
//! resuming stream as a session's, over its own route, with `changed` frames.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tole_client::{Client, Event, ReconnectPolicy, StreamItem};

const HEAD: &str =
    "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n";

/// The request head of a connection (up to the blank line).
fn read_request(s: &mut TcpStream) -> String {
    let mut buf = Vec::new();
    let mut b = [0u8; 1];
    while !buf.ends_with(b"\r\n\r\n") {
        if s.read(&mut b).unwrap() == 0 {
            break;
        }
        buf.push(b[0]);
    }
    String::from_utf8(buf).unwrap()
}

/// Serve connection `n` with `script[n]`; records every request head.
fn scripted(script: Vec<Box<dyn FnOnce(TcpStream) + Send>>) -> (String, Arc<Mutex<Vec<String>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let requests = Arc::new(Mutex::new(Vec::new()));
    let seen = Arc::clone(&requests);
    std::thread::spawn(move || {
        for step in script {
            let (mut s, _) = listener.accept().unwrap();
            seen.lock().unwrap().push(read_request(&mut s));
            step(s);
        }
    });
    (url, requests)
}

fn fast() -> ReconnectPolicy {
    ReconnectPolicy {
        backoff_initial: Duration::from_millis(1),
        idle_timeout: Duration::from_secs(10),
        ..ReconnectPolicy::default()
    }
}

fn changed_frame(seq: u64, closed: bool) -> String {
    let step = if closed { "done" } else { "check" };
    format!(
        "id: {seq}\nevent: changed\ndata: {{\"seq\":{seq},\"round\":1,\"step\":\"{step}\",\"tokens_used\":{},\"closed\":{closed}}}\n\n",
        seq * 10
    )
}

/// #1094: the stream of a discussion is the same resuming stream, over its own
/// route, with a `changed` frame in place of an entry.
#[test]
fn a_discussion_stream_resumes_after_the_last_changed_seq_and_ends_on_closed() {
    let first: Box<dyn FnOnce(TcpStream) + Send> = Box::new(|mut s| {
        let cut = &changed_frame(3, false)[..20];
        let ready =
            "event: ready\ndata: {\"id\":\"d\",\"after\":0,\"lastSeq\":2,\"busy\":true}\n\n";
        let state = "event: state\ndata: {\"state\":\"interrupted\"}\n\n";
        let wire = format!(
            "{HEAD}{}{}{ready}{state}{cut}",
            changed_frame(1, false),
            changed_frame(2, false)
        );
        s.write_all(wire.as_bytes()).unwrap();
    });
    let second: Box<dyn FnOnce(TcpStream) + Send> = Box::new(|mut s| {
        // An overlap (seq 2 again) is not delivered twice; several ledger steps
        // may have coalesced, so seq 3 never arrives and that is no loss.
        let wire = format!(
            "{HEAD}{}{}event: closed\ndata: {{\"id\":\"d\"}}\n\n",
            changed_frame(2, false),
            changed_frame(4, true),
        );
        s.write_all(wire.as_bytes()).unwrap();
    });
    let (url, requests) = scripted(vec![first, second]);
    let client = Client::connect_tcp(&url, Some("tok-123".into()), false).unwrap();

    let (mut seen, mut reconnects, mut other) = (Vec::new(), Vec::new(), Vec::new());
    let mut stream = client.follow_discussion("d", 0, fast());
    for item in stream.by_ref() {
        match item.unwrap() {
            StreamItem::Event(Event::Changed(c)) => seen.push((c.seq, c.step, c.closed)),
            StreamItem::Event(Event::Ready(r)) => other.push(format!("ready {}", r.last_seq)),
            StreamItem::Event(Event::DiscussionState { state }) => other.push(state),
            StreamItem::Event(Event::Closed { id }) => other.push(format!("closed {id}")),
            StreamItem::Reconnected { after } => reconnects.push(after),
            _ => {}
        }
    }
    let steps: Vec<_> = seen
        .iter()
        .map(|(s, step, c)| (*s, step.as_str(), *c))
        .collect();
    assert_eq!(
        steps,
        [(1, "check", false), (2, "check", false), (4, "done", true)]
    );
    assert_eq!(reconnects, [2]);
    assert_eq!(other, ["ready 2", "interrupted", "closed d"]);
    assert_eq!(stream.last_seq(), 4);
    assert!(stream.next_item().unwrap().is_none(), "ended by `closed`");

    let reqs = requests.lock().unwrap();
    assert!(
        reqs[0].starts_with("GET /discussions/d/events?after=0 HTTP/1.1"),
        "{}",
        reqs[0]
    );
    assert!(
        reqs[1].starts_with("GET /discussions/d/events?after=2 HTTP/1.1"),
        "{}",
        reqs[1]
    );
    assert!(reqs[1].contains("Authorization: Bearer tok-123\r\n"));
    assert!(reqs[1].contains("Accept: text/event-stream\r\n"));
}

/// A `changed` frame that does not decode is an error, like an entry: skipping
/// it would break the seq guarantee.
#[test]
fn a_changed_frame_that_does_not_decode_is_an_error_not_an_opaque_frame() {
    let step: Box<dyn FnOnce(TcpStream) + Send> = Box::new(|mut s| {
        let wire = format!("{HEAD}id: 1\nevent: changed\ndata: {{\"seq\":\"one\"}}\n\n");
        s.write_all(wire.as_bytes()).unwrap();
    });
    let (url, _) = scripted(vec![step]);
    let client = Client::connect_tcp(&url, None, false).unwrap();
    let mut stream = client.follow_discussion("d", 0, fast());
    assert!(stream.next_item().is_err());
    assert!(
        stream.next_item().unwrap().is_none(),
        "fused after an error"
    );
}
