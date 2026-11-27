//! #571 — over the REAL serve face, a prompt that stops for an approval
//! answers `stopReason: "approval_required"` with the queue row, the id is
//! the one `POST /approvals/{id}/decision` accepts, and every other
//! non-final outcome is still a `refusal`.

// Spawns the real `tole serve` binary (`shell-tools` feature, #349).
#![cfg(feature = "shell-tools")]

use serde_json::{json, Value};

mod common;
mod serve_harness;
use serve_harness::{new_session, spawn_mock, Serve, LEAKED};

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
    // #1023: the typed problem sits next to the row.
    let problem = json!({"kind": "approval", "subject": "write_file", "retryable": false});
    assert_eq!(body["problem"], problem, "{body}");
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
    for (text, what, kind, subject) in [
        (
            "UNKNOWN_TOOL go",
            "unknown tool",
            "unknown_tool",
            json!("no_such_tool"),
        ),
        (
            "PROVIDER_FAIL go",
            "provider failure",
            "provider",
            Value::Null,
        ),
        ("LOOP_TOOL go", "loop", "loop", json!("read_file")),
    ] {
        let id = new_session(&serve);
        let body = prompt(&serve, &id, text);
        assert_eq!(body["stopReason"], "refusal", "{what}: {body}");
        assert!(body.get("approval").is_none(), "{what}: {body}");
        // #1023: the same stop, typed (`stopReason` above is unchanged).
        let p = &body["problem"];
        assert_eq!(p["kind"], kind, "{what}: {body}");
        assert_eq!(p["subject"], subject, "{what}: {body}");
        assert_eq!(p["retryable"], kind == "provider", "{what}: {body}");
        assert_eq!(
            p.get("detail").is_some(),
            kind == "provider",
            "{what}: {body}"
        );
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
    assert_eq!(body["problem"]["kind"], "stop_gate", "{body}");
    let detail = body["problem"]["detail"].as_str().unwrap_or_default();
    assert!(detail.contains("gate says no"), "{body}");
    let (_, list) = serve.http("GET", "/approvals", None);
    assert_eq!(
        list["approvals"].as_array().map(Vec::len),
        Some(0),
        "{list}"
    );
}

/// A final answer is not a problem: no `problem` key.
#[test]
fn a_final_answer_has_no_problem() {
    let serve = Serve::spawn(&spawn_mock(0));
    let body = prompt(&serve, &new_session(&serve), "hello");
    assert_eq!(body["stopReason"], "end_turn", "{body}");
    assert!(body.get("problem").is_none(), "{body}");
}

/// #1023: the provider's error text can echo a secret of the serve's
/// environment. The `problem` detail is redacted with the session's secrets
/// before it leaves: the value is nowhere in the answer, its name stands in.
#[test]
fn a_secret_in_a_provider_error_never_leaves_in_the_problem() {
    let env = [("TOLE_TEST_API_KEY", LEAKED)];
    let serve = Serve::spawn_with_env(&spawn_mock(0), &env);
    let body = prompt(
        &serve,
        &new_session(&serve),
        "PROVIDER_FAIL PROVIDER_LEAK go",
    );
    assert_eq!(body["stopReason"], "refusal", "{body}");
    let detail = body["problem"]["detail"].as_str().expect("detail");
    assert!(detail.contains("scripted failure"), "{body}");
    assert!(
        !body.to_string().contains(LEAKED),
        "the secret left: {body}"
    );
    assert!(!detail.contains("sk-leak-"), "a head of it is left: {body}");
}

/// The same for a detail no other layer redacted: a stop gate's reason is the
/// gate's own output, so a secret it prints reaches the problem unredacted
/// unless the egress redacts it.
#[cfg(unix)]
#[test]
fn a_secret_in_a_stop_gate_reason_never_leaves_in_the_problem() {
    let dir = common::TestDir::new("tole-gate-leak");
    let gate = dir.path().join("gate.sh");
    std::fs::write(
        &gate,
        format!("#!/bin/sh\necho denied with {LEAKED}\nexit 2\n"),
    )
    .unwrap();
    let cmd = format!("/bin/sh {}", gate.display());
    let env = [("TOLE_TEST_API_KEY", LEAKED)];
    let ws = serve_harness::workspace_dir();
    std::fs::create_dir_all(&ws).unwrap();
    let serve = Serve::spawn_in_env_args(&spawn_mock(0), ws, true, &env, &["--on-turnend", &cmd]);
    let body = prompt(&serve, &new_session(&serve), "just answer");
    assert_eq!(body["problem"]["kind"], "stop_gate", "{body}");
    let detail = body["problem"]["detail"].as_str().expect("detail");
    assert!(detail.contains("denied with"), "{body}");
    assert!(
        !body.to_string().contains(LEAKED),
        "the secret left: {body}"
    );
    assert!(!detail.contains("sk-leak-"), "a head of it is left: {body}");
}
