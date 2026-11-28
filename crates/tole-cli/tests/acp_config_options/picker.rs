//! The model picker: advertisement, `session/set_config_option`, the durable override.

use crate::harness::{temp_cwd, AcpProcess};
use crate::mock::spawn_mock;
use serde_json::json;
use std::time::Duration;

/// The model picker is advertised on session/new when TOLE_MODELS is set,
/// with the env default as currentValue.
#[test]
fn session_new_advertises_model_picker() {
    let mut acp = AcpProcess::spawn_with(&[
        ("TOLE_MODEL", "model-a"),
        ("TOLE_MODELS", "model-a, model-b"),
    ]);
    acp.initialize(1);
    let cwd = temp_cwd("advertise");
    acp.send(&json!({
        "jsonrpc": "2.0", "id": 2, "method": "session/new",
        "params": {"cwd": cwd.to_string_lossy()}
    }));
    let created = acp.wait_response(2, Duration::from_secs(20));
    let opts = created["result"]["configOptions"]
        .as_array()
        .expect("configOptions advertised");
    // Approval first (priority order), model picker second (issue #176).
    assert_eq!(opts.len(), 2);
    assert_eq!(opts[0]["id"], "approval");
    let model = &opts[1];
    assert_eq!(model["id"], "model");
    assert_eq!(model["category"], "model");
    assert_eq!(model["type"], "select");
    assert_eq!(model["currentValue"], "model-a");
    assert_eq!(model["options"].as_array().expect("options").len(), 2);
}

/// Without TOLE_MODELS only the approval selector is advertised — the
/// model picker stays hidden (the host renders no model dropdown,
/// exactly pre-#176 picker behavior).
#[test]
fn no_tole_models_advertises_no_model_picker() {
    let mut acp = AcpProcess::spawn_with(&[("TOLE_MODEL", "model-a")]);
    acp.initialize(1);
    let cwd = temp_cwd("noplcker");
    acp.send(&json!({
        "jsonrpc": "2.0", "id": 2, "method": "session/new",
        "params": {"cwd": cwd.to_string_lossy()}
    }));
    let created = acp.wait_response(2, Duration::from_secs(20));
    let opts = created["result"]["configOptions"]
        .as_array()
        .expect("approval entry always advertised");
    assert_eq!(opts.len(), 1, "no model picker without TOLE_MODELS");
    assert_eq!(opts[0]["id"], "approval");
}

/// set_config_option switches the picker, validates values, and the
/// override survives a session/load in a FRESH process (durable
/// register write, not process memory).
#[test]
fn set_config_option_switches_validates_and_persists() {
    let mut acp = AcpProcess::spawn_with(&[
        ("TOLE_MODEL", "model-a"),
        ("TOLE_MODELS", "model-a,model-b"),
    ]);
    acp.initialize(1);
    let cwd = temp_cwd("switch");
    acp.send(&json!({
        "jsonrpc": "2.0", "id": 2, "method": "session/new",
        "params": {"cwd": cwd.to_string_lossy()}
    }));
    let session_id = acp.wait_response(2, Duration::from_secs(20))["result"]["sessionId"]
        .as_str()
        .unwrap()
        .to_string();

    // Switch to model-b: the response carries the FULL config state.
    acp.send(&json!({
        "jsonrpc": "2.0", "id": 3, "method": "session/set_config_option",
        "params": {"sessionId": session_id, "configId": "model", "value": "model-b"}
    }));
    let switched = acp.wait_response(3, Duration::from_secs(20));
    assert_eq!(
        switched["result"]["configOptions"][1]["currentValue"], "model-b",
        "response must carry the complete config state: {switched}"
    );

    // Invalid value / unknown configId / unknown session: JSON-RPC errors.
    for (id, params, why) in [
        (
            10u64,
            json!({"sessionId": session_id, "configId": "model", "value": "model-z"}),
            "unknown model value",
        ),
        (
            11,
            json!({"sessionId": session_id, "configId": "vibe", "value": "model-b"}),
            "unknown configId",
        ),
        (
            12,
            json!({"sessionId": "acp-does-not-exist", "configId": "model", "value": "model-b"}),
            "unknown session",
        ),
    ] {
        acp.send(&json!({
            "jsonrpc": "2.0", "id": id, "method": "session/set_config_option",
            "params": params
        }));
        let err = acp.wait_response(id, Duration::from_secs(20));
        assert!(err.get("error").is_some(), "{why} must error: {err}");
    }

    // Fresh process, session/load: the override is still model-b. The first
    // process must be gone: it holds the session's writer lock.
    drop(acp);
    let mut acp2 = AcpProcess::spawn_with(&[
        ("TOLE_MODEL", "model-a"),
        ("TOLE_MODELS", "model-a,model-b"),
    ]);
    acp2.initialize(1);
    acp2.send(&json!({
        "jsonrpc": "2.0", "id": 2, "method": "session/load",
        "params": {"cwd": cwd.to_string_lossy(), "sessionId": session_id}
    }));
    let loaded = acp2.wait_response(2, Duration::from_secs(20));
    assert_eq!(loaded["result"]["sessionId"], session_id);
    assert_eq!(
        loaded["result"]["configOptions"][1]["currentValue"], "model-b",
        "durable override must survive a fresh process: {loaded}"
    );
}

/// The switched model actually reaches the provider request body —
/// per-session (a second session without an override still uses the env
/// default), applied from the durable register on every turn.
#[test]
fn prompt_uses_switched_model_per_session() {
    let (base_url, seen, _models_hits) = spawn_mock();
    let mut acp = AcpProcess::spawn_with(&[
        ("TOLE_BASE_URL", base_url.as_str()),
        ("TOLE_MODEL", "model-a"),
        ("TOLE_MODELS", "model-a,model-b"),
        ("TOLE_API_KEY", "sk-test"),
    ]);
    acp.initialize(1);
    let cwd = temp_cwd("prompt");
    acp.send(&json!({
        "jsonrpc": "2.0", "id": 2, "method": "session/new",
        "params": {"cwd": cwd.to_string_lossy()}
    }));
    let session_id = acp.wait_response(2, Duration::from_secs(20))["result"]["sessionId"]
        .as_str()
        .unwrap()
        .to_string();
    acp.send(&json!({
        "jsonrpc": "2.0", "id": 3, "method": "session/set_config_option",
        "params": {"sessionId": session_id, "configId": "model", "value": "model-b"}
    }));
    let switched = acp.wait_response(3, Duration::from_secs(20));
    assert_eq!(
        switched["result"]["configOptions"][1]["currentValue"],
        "model-b"
    );

    acp.send(&json!({
        "jsonrpc": "2.0", "id": 4, "method": "session/prompt",
        "params": {"sessionId": session_id, "prompt": [{"type": "text", "text": "hi"}]}
    }));
    let done = acp.wait_response(4, Duration::from_secs(30));
    assert_eq!(done["result"]["stopReason"], "end_turn", "{done}");
    {
        let seen = seen.lock().unwrap();
        assert!(
            seen.iter().all(|m| m == "model-b"),
            "every provider request must carry the override: {seen:?}"
        );
    }

    // A SECOND session with no override keeps the env default — the
    // override is per-session, not process-global.
    acp.send(&json!({
        "jsonrpc": "2.0", "id": 5, "method": "session/new",
        "params": {"cwd": cwd.to_string_lossy()}
    }));
    let session2 = acp.wait_response(5, Duration::from_secs(20))["result"]["sessionId"]
        .as_str()
        .unwrap()
        .to_string();
    acp.send(&json!({
        "jsonrpc": "2.0", "id": 6, "method": "session/prompt",
        "params": {"sessionId": session2, "prompt": [{"type": "text", "text": "hi"}]}
    }));
    let done2 = acp.wait_response(6, Duration::from_secs(30));
    assert_eq!(done2["result"]["stopReason"], "end_turn", "{done2}");
    {
        let seen = seen.lock().unwrap();
        assert_eq!(seen.last().expect("second prompt"), "model-a");
    }
}

/// Regression (cora MAJOR, PR 2): flipping the approval selector must
/// reply with the session's CURRENT model — the durable override, not
/// the first advertised entry.
#[test]
fn approval_flip_reply_keeps_current_model() {
    let mut acp = AcpProcess::spawn_with(&[
        ("TOLE_MODEL", "model-a"),
        ("TOLE_MODELS", "model-a,model-b"),
    ]);
    acp.initialize(1);
    let cwd = temp_cwd("flipreply");
    acp.send(&json!({
        "jsonrpc": "2.0", "id": 2, "method": "session/new",
        "params": {"cwd": cwd.to_string_lossy()}
    }));
    let session_id = acp.wait_response(2, Duration::from_secs(20))["result"]["sessionId"]
        .as_str()
        .unwrap()
        .to_string();

    acp.send(&json!({
        "jsonrpc": "2.0", "id": 3, "method": "session/set_config_option",
        "params": {"sessionId": session_id, "configId": "model", "value": "model-b"}
    }));
    let _ = acp.wait_response(3, Duration::from_secs(20));

    acp.send(&json!({
        "jsonrpc": "2.0", "id": 4, "method": "session/set_config_option",
        "params": {"sessionId": session_id, "configId": "approval", "value": "auto"}
    }));
    let reply = acp.wait_response(4, Duration::from_secs(20));
    let opts = reply["result"]["configOptions"].as_array().unwrap();
    let model = opts
        .iter()
        .find(|o| o["id"] == "model")
        .expect("model entry");
    assert_eq!(
        model["currentValue"], "model-b",
        "approval flip must not render a stale model: {reply}"
    );
    let approval = opts.iter().find(|o| o["id"] == "approval").unwrap();
    assert_eq!(approval["currentValue"], "auto");
}
