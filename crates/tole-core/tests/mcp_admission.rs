//! #680: a server the plugin lock refuses must contribute no model-visible
//! text at all: no tools (#599), and also no resources through `mcp_read`,
//! no prompts, and no extras probe. Runs against the mock MCP server.

use serde_json::json;
use tole_core::mcp::{
    get_server_prompt, is_admitted, probe_server_extras, register_server_tools, McpReadTool,
    McpServerConfig,
};
use tole_core::plugin_lock::ManifestTool;
use tole_core::secrets::SecretSet;
use tole_core::tool::Tool;

fn server(name: &str) -> McpServerConfig {
    let script = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/mock_mcp.py");
    McpServerConfig::parse(&format!("{name}=python3 {script} full")).unwrap()
}

fn read(name: &str) -> Result<serde_json::Value, String> {
    McpReadTool::new(vec![name.to_string()]).execute(json!({"server": name, "uri": "file:///x"}))
}

#[test]
fn a_refused_server_contributes_no_model_visible_text() {
    let cfg = server("unapproved");
    let mut reg = tole_core::tool::ToolRegistry::new();
    let refuse = |_: &str, _: &str, _: &[ManifestTool]| Err("not approved".to_string());
    let names = register_server_tools(&mut reg, &cfg, &SecretSet::default(), &refuse);
    assert!(names.is_empty(), "the gate refused the tools");
    assert!(!is_admitted("unapproved"));
    let extras = probe_server_extras(&cfg);
    assert!(!extras.resources, "no resources offered");
    assert!(extras.prompts.is_empty(), "no prompts listed");
    let err = read("unapproved").unwrap_err();
    assert!(err.contains("not admitted"), "{err}");
    let err = get_server_prompt("unapproved", "demo", Default::default()).unwrap_err();
    assert!(err.contains("not admitted"), "{err}");
}

/// Control: the same server, admitted, does offer them, so the test above
/// fails for the right reason. A later refusal withdraws them again.
#[test]
fn an_admitted_server_offers_resources_and_prompts_until_refused() {
    let cfg = server("approved");
    let mut reg = tole_core::tool::ToolRegistry::new();
    let accept = |_: &str, _: &str, _: &[ManifestTool]| Ok(());
    // The fixture advertises no tools in this mode: admission is what counts.
    register_server_tools(&mut reg, &cfg, &SecretSet::default(), &accept);
    assert!(is_admitted("approved"));
    let extras = probe_server_extras(&cfg);
    assert!(extras.resources && !extras.prompts.is_empty(), "{extras:?}");
    assert!(read("approved").is_ok());

    let refuse = |_: &str, _: &str, _: &[ManifestTool]| Err("changed".to_string());
    let mut again = tole_core::tool::ToolRegistry::new();
    register_server_tools(&mut again, &cfg, &SecretSet::default(), &refuse);
    assert!(!is_admitted("approved"));
    assert!(read("approved").is_err());
}
