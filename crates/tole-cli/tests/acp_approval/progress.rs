//! Issue #196 end to end: the observer's thoughts and tool cards, and the plan updates.

use crate::harness::{temp_cwd, AcpProcess, Policy};
use crate::mock::provider_env;
use serde_json::json;
use std::time::Duration;

/// Issue #196 phases 1+2, end-to-end: during a live turn the host
/// receives the model's reasoning as an agent_thought_chunk BEFORE the
/// tool card, the tool card carries the spec kind (write_file → edit)
/// with in_progress status, and the card completes with an output
/// preview — instead of a silent "Working…".
#[test]
fn observer_streams_thought_and_tool_cards() {
    let env_pairs = provider_env().1;
    let env: Vec<(&str, &str)> = env_pairs
        .iter()
        .map(|(k, v)| (k.as_str(), v.as_str()))
        .collect();
    let acp = AcpProcess::spawn_with(&env, Policy::AllowOnce);
    acp.initialize();
    let cwd = temp_cwd("observer");
    let sid = acp.new_session(2, &cwd)["result"]["sessionId"]
        .as_str()
        .unwrap()
        .to_string();
    // Auto approval: zero permission noise, pure lifecycle observation.
    acp.send(&json!({
        "jsonrpc": "2.0", "id": 3, "method": "session/set_config_option",
        "params": {"sessionId": sid, "configId": "approval", "value": "auto"}
    }));
    let _ = acp.wait_response(3, Duration::from_secs(20));

    assert_eq!(acp.prompt(4, &sid, "WRITE_MISSION observe"), "end_turn");

    let updates = acp.updates.lock().unwrap().clone();
    let kinds: Vec<&str> = updates
        .iter()
        .filter_map(|u| u["sessionUpdate"].as_str())
        .collect();
    assert!(
        kinds.contains(&"agent_thought_chunk"),
        "reasoning must surface as a thought chunk: {kinds:?}"
    );
    let thought = updates
        .iter()
        .find(|u| u["sessionUpdate"] == "agent_thought_chunk")
        .expect("thought");
    assert_eq!(
        thought["content"]["text"], "I will act on the mission, then report.",
        "the provider's reasoning field reaches the host verbatim"
    );
    let card = updates
        .iter()
        .find(|u| u["sessionUpdate"] == "tool_call" && u["name"] == "write_file")
        .expect("tool card");
    assert_eq!(card["kind"], "edit", "write_file maps to the edit kind");
    assert_eq!(card["status"], "in_progress");
    assert_eq!(card["toolCallId"].as_str().unwrap(), "tool-1");
    let done = updates
        .iter()
        .find(|u| u["sessionUpdate"] == "tool_call_update")
        .expect("card close");
    assert_eq!(done["status"], "completed");
    assert_eq!(
        done["toolCallId"], card["toolCallId"],
        "the completion must correlate with the started card (cora MAJOR)"
    );
    assert!(
        done["content"][0]["content"]["text"]
            .as_str()
            .unwrap()
            .len()
            <= 201,
        "preview is bounded"
    );
    // Ordering: thought strictly before the tool card.
    let thought_idx = kinds
        .iter()
        .position(|k| *k == "agent_thought_chunk")
        .unwrap();
    let card_idx = kinds.iter().position(|k| *k == "tool_call").unwrap();
    assert!(thought_idx < card_idx, "thought first, action after");
}

/// Issue #196 phase 4, end-to-end: the model's update_plan call reaches
/// the host as a standard `plan` session/update (Termul PlanPanel
/// shape), with validated/normalized entries.
#[test]
fn plan_updates_reach_the_host() {
    let env_pairs = provider_env().1;
    let env: Vec<(&str, &str)> = env_pairs
        .iter()
        .map(|(k, v)| (k.as_str(), v.as_str()))
        .collect();
    let acp = AcpProcess::spawn_with(&env, Policy::AllowOnce);
    acp.initialize();
    let cwd = temp_cwd("plan");
    let sid = acp.new_session(2, &cwd)["result"]["sessionId"]
        .as_str()
        .unwrap()
        .to_string();
    acp.send(&json!({
        "jsonrpc": "2.0", "id": 3, "method": "session/set_config_option",
        "params": {"sessionId": sid, "configId": "approval", "value": "auto"}
    }));
    let _ = acp.wait_response(3, Duration::from_secs(20));

    assert_eq!(acp.prompt(4, &sid, "PLAN_MISSION plan it"), "end_turn");

    let updates = acp.updates.lock().unwrap().clone();
    let plan = updates
        .iter()
        .find(|u| u["sessionUpdate"] == "plan")
        .expect("a plan update must reach the host");
    let entries = plan["entries"].as_array().expect("entries array");
    assert_eq!(entries.len(), 2);
    assert_eq!(entries[0]["content"], "read the spec");
    assert_eq!(entries[0]["status"], "completed");
    assert_eq!(entries[1]["priority"], "medium", "spec default applied");
}
