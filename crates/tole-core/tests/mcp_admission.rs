//! #680: a server the plugin lock refuses must contribute no model-visible
//! text at all: no tools (#599), and also no resources or prompts. The
//! decision is recorded on the session's own registry, so a host offers
//! `mcp_read` and `/mcp:` prompts only for servers that registry admitted,
//! and two sessions with different locks in one process never share it.

use tole_core::mcp::{register_server_tools, McpServerConfig};
use tole_core::plugin_lock::ManifestTool;
use tole_core::secrets::SecretSet;
use tole_core::tool::ToolRegistry;

fn server(name: &str) -> McpServerConfig {
    let script = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/mock_mcp.py");
    McpServerConfig::parse(&format!("{name}=python3 {script} full")).unwrap()
}

fn accept(_: &str, _: &str, _: &[ManifestTool]) -> Result<(), String> {
    Ok(())
}

fn refuse(_: &str, _: &str, _: &[ManifestTool]) -> Result<(), String> {
    Err("not approved in the plugin lock".to_string())
}

#[test]
fn a_refused_server_is_not_admitted_by_the_registry() {
    let cfg = server("unapproved");
    let mut reg = ToolRegistry::new();
    let names = register_server_tools(&mut reg, &cfg, &SecretSet::default(), &refuse);
    assert!(names.is_empty(), "the gate refused the tools");
    assert!(
        !reg.mcp_admitted("unapproved"),
        "so the host offers it no mcp_read entry and no prompts"
    );
}

/// Two sessions in one process (serve, the cron loop): one lock admits the
/// server, the other refuses it. Neither decision leaks into the other, and
/// a later refusal in the same session withdraws the admission.
#[test]
fn admission_is_per_registry_and_a_refusal_withdraws_it() {
    let cfg = server("shared");
    let (mut a, mut b) = (ToolRegistry::new(), ToolRegistry::new());
    register_server_tools(&mut a, &cfg, &SecretSet::default(), &accept);
    register_server_tools(&mut b, &cfg, &SecretSet::default(), &refuse);
    assert!(a.mcp_admitted("shared"), "session A's lock approved it");
    assert!(!b.mcp_admitted("shared"), "session B's lock refused it");
    // The admitted server really offers what the host would then expose.
    let extras = tole_core::mcp::probe_server_extras(&cfg);
    assert!(extras.resources && !extras.prompts.is_empty(), "{extras:?}");

    register_server_tools(&mut a, &cfg, &SecretSet::default(), &refuse);
    assert!(!a.mcp_admitted("shared"), "a later refusal withdraws it");
}
