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

#[test]
fn another_sessions_approval_is_not_returned() {
    let serve = Serve::spawn(&spawn_mock(0));
    let a = new_session(&serve);
    let b = new_session(&serve);
    let first = prompt(&serve, &a, "TOOL_MISSION write it");
    let second = prompt(&serve, &b, "TOOL_MISSION write it");
    assert_eq!(first["approval"]["sessionId"], a.as_str(), "{first}");
    assert_eq!(second["approval"]["sessionId"], b.as_str(), "{second}");
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
