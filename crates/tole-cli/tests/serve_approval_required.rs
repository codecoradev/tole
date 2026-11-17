//! #571 — over the REAL serve face, a prompt that stops for an approval
//! answers `stopReason: "approval_required"` with the queue row, the id is
//! the one `POST /approvals/{id}/decision` accepts, and every other
//! non-final outcome is still a `refusal`.

// Spawns the real `tole serve` binary (`shell-tools` feature, #349).
#![cfg(feature = "shell-tools")]

use serde_json::{json, Value};

mod common;
mod serve_harness;
use serve_harness::{new_session, spawn_mock, Serve};

fn prompt(serve: &Serve, id: &str, text: &str) -> Value {
    let (code, body) = serve.http(
        "POST",
        &format!("/sessions/{id}/prompt"),
        Some(&json!({"text": text}).to_string()),
    );
    assert_eq!(code, 200, "{body}");
    body
}

#[test]
fn a_write_awaiting_approval_answers_approval_required_with_the_row() {
    let serve = Serve::spawn(&spawn_mock(0));
    let id = new_session(&serve);
    let body = prompt(&serve, &id, "TOOL_MISSION write it");
    assert_eq!(body["stopReason"], "approval_required", "{body}");
    let row = &body["approval"];
    assert_eq!(row["tool"], "write_file", "{body}");
    assert_eq!(row["status"], "pending", "{body}");
    assert_eq!(row["sessionId"], id.as_str(), "{body}");
    let approval = row["id"].as_str().expect("approval id").to_string();

    // Same row as GET /approvals.
    let (_, list) = serve.http("GET", &format!("/approvals?session={id}"), None);
    let listed = list["approvals"]
        .as_array()
        .unwrap()
        .iter()
        .find(|a| a["id"] == approval.as_str())
        .cloned();
    assert!(listed.is_some(), "{list}");

    // The id is the one the decision route accepts.
    let (code, decision) = serve.http(
        "POST",
        &format!("/approvals/{approval}/decision"),
        Some(&json!({"decision": "deny"}).to_string()),
    );
    assert_eq!(code, 200, "{decision}");
    assert_eq!(decision["status"], "denied");
}

/// Two sessions stop at overlapping times: A's provider call is slow, B
/// queues its approval meanwhile, then A stops. A's answer must name A's
/// own entry, never B's (the newest entry in the queue).
#[test]
fn another_sessions_approval_is_not_returned() {
    let serve = Serve::spawn(&spawn_mock(1500));
    let a = new_session(&serve);
    let b = new_session(&serve);
    let (first, second) = std::thread::scope(|s| {
        let slow = s.spawn(|| prompt(&serve, &a, "slow TOOL_MISSION write it"));
        // A has taken its queue mark and is waiting on the provider.
        std::thread::sleep(std::time::Duration::from_millis(400));
        let second = prompt(&serve, &b, "TOOL_MISSION write it");
        (slow.join().unwrap(), second)
    });
    assert_eq!(second["approval"]["sessionId"], b.as_str(), "{second}");
    assert_eq!(first["approval"]["sessionId"], a.as_str(), "{first}");
    assert_ne!(first["approval"]["id"], second["approval"]["id"]);
}

#[test]
fn the_other_non_final_outcomes_are_still_refusals() {
    let serve = Serve::spawn(&spawn_mock(0));
    for (text, what) in [
        ("UNKNOWN_TOOL go", "unknown tool"),
        ("PROVIDER_FAIL go", "provider failure"),
        ("LOOP_TOOL go", "loop"),
    ] {
        let id = new_session(&serve);
        let body = prompt(&serve, &id, text);
        assert_eq!(body["stopReason"], "refusal", "{what}: {body}");
        assert!(body.get("approval").is_none(), "{what}: {body}");
    }
    // None of them queued an approval.
    let (_, list) = serve.http("GET", "/approvals", None);
    assert_eq!(
        list["approvals"].as_array().map(Vec::len),
        Some(0),
        "{list}"
    );
}

/// A blocked stop gate (`--on-turnend`, denied three times) is a refusal,
/// not an approval stop, and queues nothing.
#[cfg(unix)]
#[test]
fn a_blocked_stop_gate_is_still_a_refusal() {
    let dir = common::TestDir::new("tole-gate");
    let gate = dir.path().join("gate.sh");
    std::fs::write(&gate, "#!/bin/sh\necho gate says no\nexit 2\n").unwrap();
    let cmd = format!("/bin/sh {}", gate.display());
    let serve = Serve::spawn_with_args(&spawn_mock(0), &["--on-turnend", &cmd]);
    let id = new_session(&serve);
    let body = prompt(&serve, &id, "just answer");
    assert_eq!(body["stopReason"], "refusal", "{body}");
    assert!(body.get("approval").is_none(), "{body}");
    let (_, list) = serve.http("GET", "/approvals", None);
    assert_eq!(
        list["approvals"].as_array().map(Vec::len),
        Some(0),
        "{list}"
    );
}
