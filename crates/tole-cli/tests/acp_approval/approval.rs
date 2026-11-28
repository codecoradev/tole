//! The `approval` config option, `allow_always` and the Destructive never-remembered rule.

use crate::harness::{temp_cwd, AcpProcess, Policy};
use crate::mock::provider_env;
use serde_json::json;
use std::sync::atomic::Ordering;
use std::time::Duration;

/// The approval selector is always advertised (ask default) and
/// switchable via session/set_config_option; bogus values error.
#[test]
fn approval_option_advertised_and_switchable() {
    let acp = AcpProcess::spawn_with(&[], Policy::AllowOnce);
    acp.initialize();
    let cwd = temp_cwd("switch");
    let created = acp.new_session(2, &cwd);
    let sid = created["result"]["sessionId"].as_str().unwrap().to_string();

    let opts = created["result"]["configOptions"]
        .as_array()
        .unwrap_or_else(|| panic!("no configOptions in {}", created["result"]));
    assert_eq!(opts[0]["id"], "approval");
    assert_eq!(opts[0]["category"], "mode");
    assert_eq!(opts[0]["currentValue"], "ask");

    acp.send(&json!({
        "jsonrpc": "2.0", "id": 3, "method": "session/set_config_option",
        "params": {"sessionId": sid, "configId": "approval", "value": "auto"}
    }));
    let switched = acp.wait_response(3, Duration::from_secs(20));
    assert_eq!(
        switched["result"]["configOptions"][0]["currentValue"],
        "auto"
    );

    acp.send(&json!({
        "jsonrpc": "2.0", "id": 4, "method": "session/set_config_option",
        "params": {"sessionId": sid, "configId": "approval", "value": "yolo"}
    }));
    assert!(acp
        .wait_response(4, Duration::from_secs(20))
        .get("error")
        .is_some());

    acp.send(&json!({
        "jsonrpc": "2.0", "id": 5, "method": "session/set_config_option",
        "params": {"sessionId": "acp-nope", "configId": "approval", "value": "auto"}
    }));
    assert!(acp
        .wait_response(5, Duration::from_secs(20))
        .get("error")
        .is_some());
}

/// approval=auto is the session-scoped `--yes`: the write executes with
/// ZERO permission requests, and the file really lands.
#[test]
fn auto_approval_runs_writes_without_permission() {
    let env_pairs = provider_env().1;
    let env: Vec<(&str, &str)> = env_pairs
        .iter()
        .map(|(k, v)| (k.as_str(), v.as_str()))
        .collect();
    let acp = AcpProcess::spawn_with(&env, Policy::AllowOnce);
    acp.initialize();
    let cwd = temp_cwd("auto");
    let sid = acp.new_session(2, &cwd)["result"]["sessionId"]
        .as_str()
        .unwrap()
        .to_string();
    acp.send(&json!({
        "jsonrpc": "2.0", "id": 3, "method": "session/set_config_option",
        "params": {"sessionId": sid, "configId": "approval", "value": "auto"}
    }));
    let _ = acp.wait_response(3, Duration::from_secs(20));

    assert_eq!(acp.prompt(4, &sid, "WRITE_MISSION please"), "end_turn");
    assert_eq!(
        acp.permission_count.load(Ordering::SeqCst),
        0,
        "auto approval must not surface permission requests"
    );
    assert!(cwd.join("mission-1.txt").exists(), "the write must execute");
}

/// allow_always is offered for Write calls and remembered within the
/// session: the SECOND write of the same tool prompts nothing more.
#[test]
fn allow_always_remembered_within_session() {
    let env_pairs = provider_env().1;
    let env: Vec<(&str, &str)> = env_pairs
        .iter()
        .map(|(k, v)| (k.as_str(), v.as_str()))
        .collect();
    let acp = AcpProcess::spawn_with(&env, Policy::AllowAlways);
    acp.initialize();
    let cwd = temp_cwd("always");
    let sid = acp.new_session(2, &cwd)["result"]["sessionId"]
        .as_str()
        .unwrap()
        .to_string();

    assert_eq!(acp.prompt(3, &sid, "WRITE_MISSION one"), "end_turn");
    assert_eq!(acp.permission_count.load(Ordering::SeqCst), 1);
    assert!(cwd.join("mission-1.txt").exists());
    // The offered options must include the allow_always kind.
    assert!(acp.offered_kinds.lock().unwrap()[0]
        .iter()
        .any(|k| k == "allow_always"));

    // Same tool again: remembered, no new permission request.
    assert_eq!(acp.prompt(4, &sid, "SECOND_WRITE two"), "end_turn");
    assert_eq!(
        acp.permission_count.load(Ordering::SeqCst),
        1,
        "allow_always must remember the tool for the session"
    );
    assert!(cwd.join("mission-2.txt").exists());
}

/// Destructive calls never offer allow_always and are never remembered
/// — even when the client echoes the option back (hostile/buggy
/// client): each call prompts again.
#[test]
fn destructive_never_remembered() {
    let env_pairs = provider_env().1;
    let env: Vec<(&str, &str)> = env_pairs
        .iter()
        .map(|(k, v)| (k.as_str(), v.as_str()))
        .collect();
    let acp = AcpProcess::spawn_with(&env, Policy::AllowAlways);
    acp.initialize();
    let cwd = temp_cwd("destr");
    std::fs::write(cwd.join("victim.txt"), "x").unwrap();
    let sid = acp.new_session(2, &cwd)["result"]["sessionId"]
        .as_str()
        .unwrap()
        .to_string();

    assert_eq!(acp.prompt(3, &sid, "DELETE_MISSION 1"), "end_turn");
    assert_eq!(acp.prompt(4, &sid, "DELETE_MISSION 2"), "end_turn");
    let count = acp.permission_count.load(Ordering::SeqCst);
    assert_eq!(count, 2, "each Destructive call must prompt again");
    for kinds in acp.offered_kinds.lock().unwrap().iter() {
        assert!(
            !kinds.iter().any(|k| k == "allow_always"),
            "Destructive must not offer allow_always: {kinds:?}"
        );
    }
}
