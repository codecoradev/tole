//! #1040 contract test: the typed `tole-client` against the REAL serve face
//! (the `tole` binary, a scripted provider mock).
//!
//! The client's types are its own, so drift is caught here: a response that
//! the client decodes must serialize back to exactly the JSON the server sent
//! (a field the client does not model, or models under another name, fails).

// Spawns the real `tole serve` binary (`shell-tools` feature, #349).
#![cfg(feature = "shell-tools")]

use serde::Serialize;
use serde_json::{json, Value};
use std::sync::mpsc;
use std::time::Duration;
use tole_client::{
    Client, Decision, Error, Event, IndexQuery, NewSession, PromptRequest, ReconnectPolicy, Route,
    StreamItem,
};

mod common;
mod serve_harness;
use serve_harness::{spawn_mock, Serve};

const WAIT: Duration = Duration::from_secs(20);

fn client(serve: &Serve) -> Client {
    let url = format!("http://127.0.0.1:{}", serve.port);
    Client::connect_tcp(&url, Some("t0ken".into()), false).unwrap()
}

/// `typed` is what the client made of the server's answer; it must carry all
/// of `raw` and nothing else.
fn lossless<T: Serialize>(what: &str, typed: &T, raw: &Value) {
    assert_eq!(&serde_json::to_value(typed).unwrap(), raw, "{what}");
}

fn raw(serve: &Serve, method: &str, path: &str, body: Option<&str>) -> Value {
    let (code, v) = serve.http(method, path, body);
    assert_eq!(code, 200, "{method} {path}: {v}");
    v
}

fn is_final_answer(e: &Event) -> bool {
    matches!(e, Event::Entry(e) if e.payload["text"] == "ENTRIES-ANSWER")
}

#[test]
fn create_prompt_stream_and_entries_round_trip_through_the_client() {
    let serve = Serve::spawn(&spawn_mock(0));
    let c = client(&serve);

    let health = c.health().unwrap();
    // The protocol and the binary version serve reports (#1043) are decoded,
    // not dropped.
    assert_eq!(
        health.protocol,
        Some(tole_cli::protocol::SERVE_PROTOCOL_VERSION)
    );
    assert_eq!(health.version.as_deref(), Some(env!("CARGO_PKG_VERSION")));
    lossless("health", &health, &raw(&serve, "GET", "/health", None));
    let models = c.models().unwrap();
    assert_eq!(models.default.as_deref(), Some("mock-model"));
    lossless("models", &models, &raw(&serve, "GET", "/models", None));

    let opened = c.create_session(&NewSession::default()).unwrap();
    let id = opened.session_id.clone();
    assert!(opened.workspace.is_some() && opened.resumed.is_none());
    let list = c.sessions().unwrap();
    assert!(list.sessions.iter().any(|s| s.id == id && !s.busy));
    lossless("sessions", &list, &raw(&serve, "GET", "/sessions", None));

    // Open the stream and wait for `ready`: only then does the server attach
    // an observer to the turn (a stream nobody reads costs nothing).
    let (tx, rx) = mpsc::channel();
    let follower = c.clone();
    let sid = id.clone();
    std::thread::spawn(move || {
        for item in follower.follow_events(&sid, 0, ReconnectPolicy::default()) {
            if tx.send(item).is_err() {
                return;
            }
        }
    });
    let mut seen = Vec::new();
    loop {
        match rx.recv_timeout(WAIT).expect("a ready frame").unwrap() {
            StreamItem::Event(Event::Ready(r)) => {
                assert_eq!((r.id.as_str(), r.after, r.busy), (id.as_str(), 0, false));
                break;
            }
            other => seen.push(other),
        }
    }

    let reply = c.prompt(&id, &PromptRequest::text("hello")).unwrap();
    assert_eq!(reply.text.as_deref(), Some("ENTRIES-ANSWER"), "{reply:?}");
    assert!(reply.approval.is_none());

    // The text delta arrives before the durable final answer; entries come in
    // seq order, each once.
    let mut events = Vec::new();
    loop {
        match rx
            .recv_timeout(WAIT)
            .expect("the final answer entry")
            .unwrap()
        {
            StreamItem::Event(e) => {
                let done = is_final_answer(&e);
                events.push(e);
                if done {
                    break;
                }
            }
            other => seen.push(other),
        }
    }
    let delta = events
        .iter()
        .position(|e| matches!(e, Event::TextDelta { text } if text == "ENTRIES-ANSWER"));
    assert!(delta.is_some_and(|d| d < events.len() - 1), "{events:?}");
    let seqs: Vec<u64> = events
        .iter()
        .filter_map(|e| {
            if let Event::Entry(e) = e {
                Some(e.seq)
            } else {
                None
            }
        })
        .collect();
    assert!(
        seqs.len() >= 2 && seqs.windows(2).all(|w| w[0] < w[1]),
        "{seqs:?}"
    );

    // The transcript read agrees with the stream.
    let page = c.entries(&id, 0, None).unwrap();
    assert_eq!(page.entries.iter().map(|e| e.seq).collect::<Vec<_>>(), seqs);
    lossless(
        "entries",
        &page,
        &raw(&serve, "GET", &format!("/sessions/{id}/entries"), None),
    );
    let first = c.entries(&id, 0, Some(1)).unwrap();
    assert!(first.has_more && first.entries.len() == 1);
    assert!(c
        .entries(&id, page.next_after, None)
        .unwrap()
        .entries
        .is_empty());

    let status = c.status(&id).unwrap();
    assert!(!status.busy && status.entries == Some(page.entries.len() as u64));
    // `snapshotAt` moves between reads: compare the field set, not the values.
    let raw_status = raw(&serve, "GET", &format!("/sessions/{id}/status"), None);
    let keys = |v: &Value| v.as_object().unwrap().keys().cloned().collect::<Vec<_>>();
    assert_eq!(
        keys(&serde_json::to_value(&status).unwrap()).len(),
        keys(&raw_status).len()
    );
    for k in keys(&raw_status) {
        assert!(
            serde_json::to_value(&status).unwrap().get(&k).is_some(),
            "status lost `{k}`"
        );
    }

    let index = c.session_index(&IndexQuery::default()).unwrap();
    assert!(index
        .sessions
        .iter()
        .any(|s| s.id == id && s.live && s.turns >= 1));
    lossless(
        "index",
        &index,
        &raw(&serve, "GET", "/sessions/index", None),
    );

    // Resume of a live session; cancel of an idle one.
    let again = c.resume_session(&id).unwrap();
    assert_eq!(
        (again.already_live, again.resumed),
        (Some(true), Some(false))
    );
    lossless(
        "resume",
        &again,
        &raw(
            &serve,
            "POST",
            "/sessions",
            Some(&json!({"resume": id}).to_string()),
        ),
    );
    let cancel = c.cancel(&id).unwrap();
    assert!(cancel.cancelled);
    lossless(
        "cancel",
        &cancel,
        &raw(&serve, "POST", &format!("/sessions/{id}/cancel"), None),
    );

    // Refusals are typed errors.
    assert!(matches!(
        c.status("nope"),
        Err(Error::Status { status: 404, .. })
    ));
    let wrong = Client::connect_tcp(
        &format!("http://127.0.0.1:{}", serve.port),
        Some("bad".into()),
        false,
    )
    .unwrap();
    assert!(matches!(
        wrong.sessions(),
        Err(Error::Status { status: 401, .. })
    ));
}

#[test]
fn an_approval_is_listed_decided_and_resumed_through_the_client() {
    let serve = Serve::spawn(&spawn_mock(0));
    let c = client(&serve);
    let id = c.create_session(&NewSession::default()).unwrap().session_id;

    let reply = c
        .prompt(&id, &PromptRequest::text("TOOL_MISSION write it"))
        .unwrap();
    assert_eq!(reply.stop_reason, "approval_required", "{reply:?}");
    let row = reply.approval.expect("the queue row");
    assert_eq!(
        (row.tool.as_str(), row.status.as_str()),
        ("write_file", "pending")
    );

    let list = c.approvals(Some(&id)).unwrap();
    assert_eq!(list.approvals.len(), 1);
    assert_eq!(list.approvals[0].id, row.id);
    lossless(
        "approvals",
        &list,
        &raw(&serve, "GET", &format!("/approvals?session={id}"), None),
    );

    let decided = c.decide(&row.id, Decision::Allow).unwrap();
    assert_eq!(
        (decided.status.as_str(), decided.resuming),
        ("approved", Some(true))
    );
    let work = serve.dir.join("approved.txt");
    common::wait_until(common::WAIT, "the approved write to land", || work.exists());
    // The opposite verdict after the fact is a conflict, never a silent flip.
    assert!(matches!(
        c.decide(&row.id, Decision::Deny),
        Err(Error::Status { status: 409, .. })
    ));
}

/// Every route the client has a method for exists on the server: a rename or
/// removal there fails here. (The reverse, a server route with no client
/// method, is the next slice: the client covers the desktop's first set.)
#[test]
fn every_client_route_exists_on_the_server() {
    let serve = Serve::spawn(&spawn_mock(0));
    for route in Route::ALL {
        let path = route.template().replace("{id}", "nope");
        let body = (route.method() == "POST").then_some("{}");
        let (code, v) = serve.http(route.method(), &path, body);
        let route_missing = code == 404 && v["error"] == "not found";
        assert!(
            !route_missing,
            "{route:?}: the server has no `{} {}`",
            route.method(),
            route.template()
        );
    }
}
