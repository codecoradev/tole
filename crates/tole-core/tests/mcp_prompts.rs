//! MCP prompts + resources client tests (issue #213). The live cases talk to
//! `fixtures/mock_mcp.py` over stdio (python3 is on every CI runner).

use serde_json::json;
use tole_core::mcp::{
    get_server_prompt, parse_prompt_command, probe_server_extras, McpReadTool, McpServerConfig,
};
use tole_core::tool::{Risk, Tool};

fn mock(name: &str, mode: &str) -> McpServerConfig {
    let script = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/mock_mcp.py");
    McpServerConfig::parse(&format!("{name}=python3 {script} {mode}")).unwrap()
}

#[test]
fn prompt_command_parses_server_prompt_and_args() {
    let (s, p, a) = parse_prompt_command("/mcp:cora:review topic=auth depth=2")
        .unwrap()
        .unwrap();
    assert_eq!((s.as_str(), p.as_str()), ("cora", "review"));
    assert_eq!(a.get("topic").map(String::as_str), Some("auth"));
    assert_eq!(a.get("depth").map(String::as_str), Some("2"));
}

#[test]
fn prompt_command_ignores_other_lines_and_rejects_malformed() {
    assert!(parse_prompt_command("hello").is_none());
    assert!(parse_prompt_command("/status").is_none());
    assert!(parse_prompt_command("/mcp:onlyserver").unwrap().is_err());
    assert!(parse_prompt_command("/mcp:a:b notkv").unwrap().is_err());
}

#[test]
fn mcp_read_is_read_only_and_rejects_unlisted_servers() {
    let t = McpReadTool::new(vec!["a".into()]);
    assert_eq!(t.name(), "mcp_read");
    assert_eq!(t.risk(), Risk::ReadOnly);
    let e = t
        .execute(json!({"server": "other", "uri": "file:///x"}))
        .unwrap_err();
    assert!(e.contains("not offer resources"), "{e}");
}

#[test]
fn live_server_prompts_and_resources_are_probed_rendered_and_read() {
    let cfg = mock("mockfull", "full");
    let extras = probe_server_extras(&cfg);
    assert!(extras.resources);
    assert_eq!(extras.prompts.len(), 1);
    assert_eq!(extras.prompts[0].name, "review");
    assert_eq!(extras.prompts[0].arguments, vec!["topic"]);

    let args = [("topic".to_string(), "auth".to_string())].into();
    assert_eq!(
        get_server_prompt("mockfull", "review", args).unwrap(),
        "Review: auth"
    );

    let out = McpReadTool::new(vec!["mockfull".into()])
        .execute(json!({"server": "mockfull", "uri": "file:///demo"}))
        .unwrap();
    assert_eq!(out["result"], "demo body");
}

#[test]
fn server_without_capabilities_contributes_nothing() {
    let extras = probe_server_extras(&mock("mockbare", "bare"));
    assert!(extras.prompts.is_empty());
    assert!(!extras.resources);
}
