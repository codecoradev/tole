//! #1024: golden transcripts of `tole serve`. `GET /sessions/{id}/entries`
//! returns exactly the `Entry` the append-only JSONL holds (#524), so these
//! files are at once the API a client reads and a tripwire for the ledger
//! format: a payload key that is renamed or dropped fails here before it
//! reaches a client or an old session file. Each entry kind the log writes
//! (`message`, `intent`, `tool_result`, `error`) appears at least once, and the
//! raw JSONL lines of one session are pinned next to the wire form.

// Spawns the real `tole serve` binary (`shell-tools` feature, #349).
#![cfg(feature = "shell-tools")]

use serde_json::{json, Value};
use std::collections::BTreeSet;
use std::time::Duration;

mod common;
mod serve_golden;
mod serve_harness;
use serve_golden::{check, renumber, Volatile};
use serve_harness::{new_session, spawn_mock, Frame, Serve, Sse};

const WAIT: Duration = Duration::from_secs(20);

fn post(serve: &Serve, path: &str, body: Value) -> (u16, Value) {
    serve.http("POST", path, Some(&body.to_string()))
}

fn entries(serve: &Serve, id: &str) -> Vec<Value> {
    let (code, page) = serve.http("GET", &format!("/sessions/{id}/entries"), None);
    assert_eq!(code, 200, "{page}");
    assert_eq!(page["hasMore"], false, "one page holds the scenario");
    page["entries"].as_array().unwrap().clone()
}

/// Wait until the session is idle and its log has an assistant answer.
fn settled(serve: &Serve, id: &str) -> Vec<Value> {
    common::wait_for(WAIT, "a settled turn", || {
        let (_, st) = serve.http("GET", &format!("/sessions/{id}/status"), None);
        let log = entries(serve, id);
        let done = log
            .last()
            .is_some_and(|e| e["type"] == "message" && e["payload"]["role"] == "assistant");
        (st["busy"] == false && done).then_some(log)
    })
}

/// A session whose Write was asked for, then decided (`allow` or `deny`).
fn decided(serve: &Serve, verdict: &str) -> String {
    let id = new_session(serve);
    let (code, body) = post(
        serve,
        &format!("/sessions/{id}/prompt"),
        json!({"text": "TOOL_MISSION write it"}),
    );
    assert_eq!(
        (code, &body["stopReason"]),
        (200, &json!("approval_required"))
    );
    let approval = body["approval"]["id"].as_str().unwrap();
    let (code, body) = post(
        serve,
        &format!("/approvals/{approval}/decision"),
        json!({"decision": verdict}),
    );
    assert!(code == 200 || code == 202, "{code} {body}");
    id
}

/// A turn that ends in the given way, whatever the verdict.
fn ran(serve: &Serve, marker: &str) -> String {
    let id = new_session(serve);
    let (code, body) = post(
        serve,
        &format!("/sessions/{id}/prompt"),
        json!({"text": marker}),
    );
    assert_eq!(code, 200, "{body}");
    id
}

/// The session file as a ledger: the header, every `entry` line in full, and
/// the other kinds of line (`state`, `usage`, `register`) by the keys they carry
/// (their values are the state machine's and the usage ledger's own business;
/// a key that goes missing from one is a format change).
fn ledger(serve: &Serve, id: &str) -> Value {
    let text = std::fs::read_to_string(serve.session_file(id)).unwrap();
    let mut header = Value::Null;
    let mut entries = Vec::new();
    let mut shapes: Vec<Value> = Vec::new();
    for line in text.lines() {
        let v: Value = serde_json::from_str(line).unwrap();
        // A commit is written as one line holding several records.
        let records = match v {
            Value::Array(a) => a,
            one => vec![one],
        };
        for r in records {
            match r["kind"].as_str().unwrap() {
                "header" => header = r,
                "entry" => entries.push(r),
                kind => {
                    let mut keys: Vec<&String> = r.as_object().unwrap().keys().collect();
                    keys.sort();
                    let shape = json!({"kind": kind, "keys": keys});
                    if !shapes.contains(&shape) {
                        shapes.push(shape);
                    }
                }
            }
        }
    }
    json!({"header": header, "entries": entries, "otherLines": shapes})
}

#[test]
fn every_entry_kind_keeps_its_wire_shape() {
    let serve = Serve::spawn(&spawn_mock(0));
    let mut kinds = BTreeSet::new();
    let scenarios: Vec<(&str, String)> = vec![
        ("allowed_write", decided(&serve, "allow")),
        ("denied_write", decided(&serve, "deny")),
        ("unknown_tool", ran(&serve, "UNKNOWN_TOOL please")),
        ("failing_read", ran(&serve, "LOOP_TOOL please")),
        ("plain_answer", ran(&serve, "hello")),
    ];
    for (name, id) in &scenarios {
        // An allow resumes the turn on a server thread; the rest are settled.
        let log = if *name == "allowed_write" {
            settled(&serve, id)
        } else {
            entries(&serve, id)
        };
        for e in &log {
            kinds.insert(e["type"].as_str().unwrap().to_string());
        }
        let vol = Volatile::of(&serve, &[(id, "<SESSION>")]);
        let wire = json!({"id": id, "entries": log});
        check(&format!("entries/{name}"), &renumber(&vol.apply(&wire)));
        if *name == "allowed_write" {
            check(
                "ledger/allowed_write",
                &renumber(&vol.apply(&ledger(&serve, id))),
            );
        }
    }
    // The four kinds `tole_core::entry::EntryType` names are all on the wire.
    let want: BTreeSet<String> = ["error", "intent", "message", "tool_result"]
        .map(String::from)
        .into();
    assert_eq!(
        kinds, want,
        "a scenario no longer produces a kind (or a new one appeared)"
    );
}

/// One sample of each kind of frame a client reads from the event stream, in
/// the kinds sorted by name. The durable `entry` frames carry the
/// `seq` as the SSE id; the others are best-effort and have none. The frames
/// the mock turn cannot provoke (`reasoning`, `resync`, `closed`) are pinned by
/// the committed schema of `tole-client` only.
#[test]
fn event_frames_keep_their_shape() {
    let serve = Serve::spawn(&spawn_mock(0));
    let id = new_session(&serve);
    let sse = Sse::open(&serve, &format!("/sessions/{id}/events"), "", true).unwrap();
    let answer = |f: &Frame| f.event == "entry" && f.data["payload"]["text"] == "ENTRIES-ANSWER";
    let mut frames = sse.until(WAIT, |f| f.event == "ready");

    // A turn that streams text.
    let (code, body) = post(
        &serve,
        &format!("/sessions/{id}/prompt"),
        json!({"text": "hello"}),
    );
    assert_eq!(code, 200, "{body}");
    frames.extend(sse.until(WAIT, answer));

    // A Write that waits for a decision, then runs.
    let (code, body) = post(
        &serve,
        &format!("/sessions/{id}/prompt"),
        json!({"text": "TOOL_MISSION write it"}),
    );
    assert_eq!(code, 200, "{body}");
    let approval = body["approval"]["id"].as_str().unwrap().to_string();
    // The request frame is read BEFORE the decision, while the row is pending.
    frames.extend(sse.until(WAIT, |f| f.event == "approval_requested"));
    let (code, _) = post(
        &serve,
        &format!("/approvals/{approval}/decision"),
        json!({"decision": "allow"}),
    );
    assert_eq!(code, 202);
    frames.extend(sse.until(WAIT, |f| f.event == "approval_resolved"));
    frames.extend(sse.until(WAIT, answer));

    // A read-only tool call gives the tool frames.
    let (code, _) = post(
        &serve,
        &format!("/sessions/{id}/prompt"),
        json!({"text": "LOOP_TOOL x"}),
    );
    assert_eq!(code, 200);
    frames.extend(sse.until(WAIT, |f| f.event == "tool_finished"));

    let mut samples: Vec<Value> = Vec::new();
    let mut named: BTreeSet<String> = BTreeSet::new();
    for f in &frames {
        if named.insert(f.event.clone()) {
            samples.push(json!({"event": f.event, "hasSseId": f.id.is_some(), "data": f.data}));
        }
    }
    // Text and entry frames race each other; the order of the kinds is not a contract.
    samples.sort_by_key(|s| s["event"].as_str().unwrap().to_string());
    let vol = Volatile::of(&serve, &[(&id, "<SESSION>"), (&approval, "<APPROVAL>")]);
    check("events/frames", &renumber(&vol.apply(&json!(samples))));
}
