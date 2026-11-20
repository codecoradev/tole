//! The event stream against a scripted server: incremental delivery, and a
//! forced disconnect that resumes with no lost and no duplicated frame.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tole_client::{Client, Error, Event, ReconnectPolicy, StreamItem};

const HEAD: &str =
    "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n";

fn entry_frame(seq: u64) -> String {
    format!(
        "id: {seq}\nevent: entry\ndata: {{\"seq\":{seq},\"id\":\"e_{seq}\",\"parentId\":null,\"type\":\"message\",\"timestamp\":0,\"payload\":{{\"text\":\"t{seq}\"}}}}\n\n"
    )
}

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

#[test]
fn a_forced_disconnect_resumes_after_the_last_seq_without_loss_or_duplicates() {
    let first: Box<dyn FnOnce(TcpStream) + Send> = Box::new(|mut s| {
        let cut = &entry_frame(3)[..20]; // dies in the middle of entry 3
        let wire = format!(
            "{HEAD}{}{}event: text_delta\ndata: {{\"text\":\"x\"}}\n\n{cut}",
            entry_frame(1),
            entry_frame(2)
        );
        s.write_all(wire.as_bytes()).unwrap();
        // dropped: the connection closes with a frame half written
    });
    let second: Box<dyn FnOnce(TcpStream) + Send> = Box::new(|mut s| {
        // A server may overlap (entry 2 again): it must not be delivered twice.
        let wire = format!(
            "{HEAD}{}{}{}event: closed\ndata: {{\"id\":\"s\"}}\n\n",
            entry_frame(2),
            entry_frame(3),
            entry_frame(4)
        );
        s.write_all(wire.as_bytes()).unwrap();
    });
    let (url, requests) = scripted(vec![first, second]);
    let client = Client::connect_tcp(&url, Some("tok-123".into()), false).unwrap();

    let mut seqs = Vec::new();
    let mut reconnects = Vec::new();
    for item in client.follow_events("s", 0, fast()) {
        match item.unwrap() {
            StreamItem::Event(Event::Entry(e)) => seqs.push(e.seq),
            StreamItem::Reconnected { after } => reconnects.push(after),
            _ => {}
        }
    }
    assert_eq!(seqs, [1, 2, 3, 4], "every entry exactly once, in order");
    assert_eq!(reconnects, [2], "one reconnect, resuming after seq 2");

    let reqs = requests.lock().unwrap();
    assert!(
        reqs[0].starts_with("GET /sessions/s/events?after=0 HTTP/1.1"),
        "{}",
        reqs[0]
    );
    assert!(
        reqs[1].starts_with("GET /sessions/s/events?after=2 HTTP/1.1"),
        "{}",
        reqs[1]
    );
    assert!(reqs[1].contains("Authorization: Bearer tok-123\r\n"));
    assert!(reqs[1].contains("Accept: text/event-stream\r\n"));
}

#[test]
fn frames_reach_the_caller_as_they_arrive_over_a_chunked_body() {
    let (release_tx, release_rx) = mpsc::channel::<()>();
    let step: Box<dyn FnOnce(TcpStream) + Send> = Box::new(move |mut s| {
        let chunk = |t: &str| format!("{:x}\r\n{t}\r\n", t.len());
        let delta = "event: text_delta\ndata: {\"text\":\"first\"}\n\n";
        s.write_all(
            format!(
                "HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n{}",
                chunk(delta)
            )
            .as_bytes(),
        )
        .unwrap();
        // Hold the rest back until the client has the delta (bounded, so a
        // client that buffers the body cannot hang the test).
        let _ = release_rx.recv_timeout(Duration::from_secs(5));
        let rest = format!(
            "{}event: closed\ndata: {{\"id\":\"s\"}}\n\n",
            entry_frame(1)
        );
        s.write_all(format!("{}0\r\n\r\n", chunk(&rest)).as_bytes())
            .unwrap();
    });
    let (url, _) = scripted(vec![step]);
    let client = Client::connect_tcp(&url, None, false).unwrap();
    let mut stream = client.follow_events("s", 0, fast());

    let started = Instant::now();
    let first = stream.next_item().unwrap().unwrap();
    assert_eq!(
        first,
        StreamItem::Event(Event::TextDelta {
            text: "first".into()
        })
    );
    assert!(
        started.elapsed() < Duration::from_secs(3),
        "the delta waited for the rest of the body"
    );
    release_tx.send(()).unwrap();
    assert!(matches!(
        stream.next_item().unwrap(),
        Some(StreamItem::Event(Event::Entry(e))) if e.seq == 1
    ));
    assert!(matches!(
        stream.next_item().unwrap(),
        Some(StreamItem::Event(Event::Closed { .. }))
    ));
    assert!(stream.next_item().unwrap().is_none());
}

#[test]
fn a_refusal_is_an_error_not_a_retry_and_giving_up_is_bounded() {
    let refuse: Box<dyn FnOnce(TcpStream) + Send> = Box::new(|mut s| {
        let body = "{\"error\":\"unknown session\"}";
        let wire = format!(
            "HTTP/1.1 404 Not Found\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        s.write_all(wire.as_bytes()).unwrap();
    });
    let (url, _) = scripted(vec![refuse]);
    let client = Client::connect_tcp(&url, None, false).unwrap();
    match client.follow_events("nope", 0, fast()).next_item() {
        Err(Error::Status {
            status: 404,
            message,
            ..
        }) => assert_eq!(message, "unknown session"),
        other => panic!("{other:?}"),
    }

    // A server that always drops the connection: bounded retries, then an error.
    let drops: Vec<Box<dyn FnOnce(TcpStream) + Send>> = (0..3)
        .map(|_| {
            Box::new(|mut s: TcpStream| s.write_all(HEAD.as_bytes()).unwrap())
                as Box<dyn FnOnce(TcpStream) + Send>
        })
        .collect();
    let (url, _) = scripted(drops);
    let client = Client::connect_tcp(&url, None, false).unwrap();
    let policy = ReconnectPolicy {
        max_attempts: 2,
        ..fast()
    };
    let end = client
        .follow_events("s", 0, policy)
        .find_map(|i| i.err())
        .expect("the stream ends in an error");
    assert!(matches!(end, Error::ReconnectExhausted(2)), "{end:?}");
}

#[test]
fn the_stream_is_fused_after_an_error_and_never_reconnects_again() {
    // A refusal: one request, the error once, then the end.
    let refuse: Box<dyn FnOnce(TcpStream) + Send> = Box::new(|mut s| {
        let body = "{\"error\":\"unauthorized\"}";
        let wire = format!(
            "HTTP/1.1 401 Unauthorized\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        s.write_all(wire.as_bytes()).unwrap();
    });
    let (url, requests) = scripted(vec![refuse]);
    let client = Client::connect_tcp(&url, None, false).unwrap();
    let mut stream = client.follow_events("s", 0, fast());
    assert!(matches!(
        stream.next_item(),
        Err(Error::Status { status: 401, .. })
    ));
    for _ in 0..3 {
        assert!(stream.next_item().unwrap().is_none(), "fused");
    }
    assert!(stream.next().is_none(), "the iterator ends too");
    assert_eq!(
        requests.lock().unwrap().len(),
        1,
        "no reconnect after the error"
    );

    // Giving up: same, and no tight loop of connects afterwards.
    let drops: Vec<Box<dyn FnOnce(TcpStream) + Send>> = (0..2)
        .map(|_| {
            Box::new(|mut s: TcpStream| s.write_all(HEAD.as_bytes()).unwrap())
                as Box<dyn FnOnce(TcpStream) + Send>
        })
        .collect();
    let (url, requests) = scripted(drops);
    let client = Client::connect_tcp(&url, None, false).unwrap();
    let policy = ReconnectPolicy {
        max_attempts: 1,
        ..fast()
    };
    let mut stream = client.follow_events("s", 0, policy);
    let err = loop {
        match stream.next_item() {
            Err(e) => break e,
            Ok(Some(_)) => {}
            Ok(None) => panic!("ended without an error"),
        }
    };
    assert!(matches!(err, Error::ReconnectExhausted(1)), "{err:?}");
    let seen = requests.lock().unwrap().len();
    let started = Instant::now();
    for _ in 0..1000 {
        assert!(stream.next_item().unwrap().is_none());
    }
    assert!(started.elapsed() < Duration::from_secs(1));
    assert_eq!(
        requests.lock().unwrap().len(),
        seen,
        "no connect after giving up"
    );
}

#[test]
fn a_flapping_server_is_given_up_on_even_though_it_sends_ready_and_heartbeats() {
    // 200, `ready`, a heartbeat, then close: no entry ever arrives. Frames
    // that are not progress must not reset the retry budget.
    let flap = || -> Box<dyn FnOnce(TcpStream) + Send> {
        Box::new(|mut s| {
            let ready =
                "event: ready\ndata: {\"id\":\"s\",\"after\":0,\"lastSeq\":0,\"busy\":false}\n\n";
            s.write_all(format!("{HEAD}{ready}: ping\n\n").as_bytes())
                .unwrap();
        })
    };
    let (url, requests) = scripted((0..20).map(|_| flap()).collect());
    let client = Client::connect_tcp(&url, None, false).unwrap();
    let policy = ReconnectPolicy {
        max_attempts: 2,
        ..fast()
    };
    let end = client
        .follow_events("s", 0, policy)
        .find_map(|i| i.err())
        .expect("the stream gives up");
    assert!(matches!(end, Error::ReconnectExhausted(2)), "{end:?}");
    // 20 flaps were on offer: giving up after 3 connections proves the cap.
    assert_eq!(requests.lock().unwrap().len(), 3, "attempts are bounded");
}

#[test]
fn progress_or_a_long_lived_connection_resets_the_retry_budget() {
    // max_attempts 1, yet the stream survives: every drop follows either an
    // entry with a new seq or a connection that lived past backoff_max.
    let with_entry: Box<dyn FnOnce(TcpStream) + Send> = Box::new(|mut s| {
        s.write_all(format!("{HEAD}{}", entry_frame(1)).as_bytes())
            .unwrap()
    });
    let long_lived: Box<dyn FnOnce(TcpStream) + Send> = Box::new(|mut s| {
        s.write_all(format!("{HEAD}: ping\n\n").as_bytes()).unwrap();
        std::thread::sleep(Duration::from_millis(150));
    });
    let with_entry2: Box<dyn FnOnce(TcpStream) + Send> = Box::new(|mut s| {
        s.write_all(format!("{HEAD}{}", entry_frame(2)).as_bytes())
            .unwrap()
    });
    let closing: Box<dyn FnOnce(TcpStream) + Send> = Box::new(|mut s| {
        let wire = format!("{HEAD}event: closed\ndata: {{\"id\":\"s\"}}\n\n");
        s.write_all(wire.as_bytes()).unwrap();
    });
    let (url, _) = scripted(vec![with_entry, long_lived, with_entry2, closing]);
    let client = Client::connect_tcp(&url, None, false).unwrap();
    let policy = ReconnectPolicy {
        max_attempts: 1,
        backoff_max: Duration::from_millis(100),
        ..fast()
    };
    let mut seqs = Vec::new();
    for item in client.follow_events("s", 0, policy) {
        if let StreamItem::Event(Event::Entry(e)) = item.unwrap() {
            seqs.push(e.seq);
        }
    }
    assert_eq!(seqs, [1, 2]);
}

#[test]
fn a_drip_fed_refusal_body_cannot_stall_the_stream() {
    let drip: Box<dyn FnOnce(TcpStream) + Send> = Box::new(|mut s| {
        s.write_all(b"HTTP/1.1 503 Service Unavailable\r\nContent-Length: 10000\r\n\r\n")
            .unwrap();
        for _ in 0..10_000 {
            if s.write_all(b"x").is_err() {
                return;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    });
    let (url, _) = scripted(vec![drip]);
    let client = Client::connect_tcp(&url, None, false)
        .unwrap()
        .with_body_deadline(Duration::from_millis(300))
        .unwrap();
    let started = Instant::now();
    let err = client.events("s", 0, None).err().expect("a refusal");
    assert!(matches!(err, Error::Io(_)), "{err:?}");
    assert!(started.elapsed() < Duration::from_secs(5));
}

#[test]
fn an_open_stream_outlives_the_body_deadline_and_a_zero_idle_timeout_is_refused() {
    // The body deadline bounds a head and a refusal, never the stream itself.
    let slow: Box<dyn FnOnce(TcpStream) + Send> = Box::new(|mut s| {
        s.write_all(HEAD.as_bytes()).unwrap();
        std::thread::sleep(Duration::from_millis(400));
        let wire = format!(
            "{}event: closed\ndata: {{\"id\":\"s\"}}\n\n",
            entry_frame(1)
        );
        s.write_all(wire.as_bytes()).unwrap();
    });
    let (url, _) = scripted(vec![slow]);
    let client = Client::connect_tcp(&url, None, false)
        .unwrap()
        .with_body_deadline(Duration::from_millis(150))
        .unwrap();
    let seqs: Vec<u64> = client
        .follow_events("s", 0, fast())
        .filter_map(|i| match i.unwrap() {
            StreamItem::Event(Event::Entry(e)) => Some(e.seq),
            _ => None,
        })
        .collect();
    assert_eq!(seqs, [1]);

    // A zero idle timeout is a setting error, not a connection failure to retry.
    let c = Client::connect_tcp("http://127.0.0.1:1", None, false).unwrap();
    let policy = ReconnectPolicy {
        idle_timeout: Duration::ZERO,
        ..fast()
    };
    assert!(matches!(
        c.follow_events("s", 0, policy).next_item(),
        Err(Error::InvalidConfig(_))
    ));
}

#[test]
fn a_silent_server_is_a_timeout_that_the_stream_retries_and_a_malformed_one_is_not() {
    // The GET stream may be retried after a timeout (nothing can be repeated):
    // the first connection gets no answer within the idle bound.
    // (Held open on its own thread, so the server loop can take the next
    // connection.)
    let silent: Box<dyn FnOnce(TcpStream) + Send> = Box::new(|s| {
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(500));
            drop(s);
        });
    });
    let answer: Box<dyn FnOnce(TcpStream) + Send> = Box::new(|mut s| {
        let wire = format!(
            "{HEAD}{}event: closed\ndata: {{\"id\":\"s\"}}\n\n",
            entry_frame(1)
        );
        s.write_all(wire.as_bytes()).unwrap();
    });
    let (url, requests) = scripted(vec![silent, answer]);
    let client = Client::connect_tcp(&url, None, false).unwrap();
    let policy = ReconnectPolicy {
        idle_timeout: Duration::from_millis(150),
        ..fast()
    };
    let seqs: Vec<u64> = client
        .follow_events("s", 0, policy)
        .filter_map(|i| match i.unwrap() {
            StreamItem::Event(Event::Entry(e)) => Some(e.seq),
            _ => None,
        })
        .collect();
    assert_eq!(seqs, [1]);
    assert_eq!(requests.lock().unwrap().len(), 2);

    // A malformed stream (a chunk without its CRLF) is a protocol error: the
    // stream ends with it instead of reconnecting at once.
    let bad: Box<dyn FnOnce(TcpStream) + Send> = Box::new(|mut s| {
        s.write_all(b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n3\r\nabcXX")
            .unwrap();
    });
    let (url, requests) = scripted(vec![bad]);
    let client = Client::connect_tcp(&url, None, false).unwrap();
    let mut stream = client.follow_events("s", 0, fast());
    assert!(matches!(stream.next_item(), Err(Error::Protocol(_))));
    assert!(stream.next_item().unwrap().is_none(), "fused");
    assert_eq!(requests.lock().unwrap().len(), 1);
}
