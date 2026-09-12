//! MCP client tests (issue #74). The REAL stdio integration is exercised
//! live with a reference server (see PR notes); these unit tests pin the
//! config-parsing and trust-model contracts that don't need a server.

use tole_core::mcp::McpServerConfig;
use tole_core::tool::Risk;

#[test]
fn parses_name_command_args() {
    let cfg =
        McpServerConfig::parse("fs=npx -y @modelcontextprotocol/server-filesystem /tmp").unwrap();
    assert_eq!(cfg.name, "fs");
    assert_eq!(cfg.command, "npx");
    assert_eq!(
        cfg.args,
        vec!["-y", "@modelcontextprotocol/server-filesystem", "/tmp"]
    );
}

#[test]
fn rejects_malformed_specs() {
    assert!(McpServerConfig::parse("no-equals-sign").is_err());
    assert!(McpServerConfig::parse("=command").is_err());
    assert!(McpServerConfig::parse("name=").is_err());
    assert!(McpServerConfig::parse("bad name=cmd").is_err());
}

#[test]
fn valid_names_accepted() {
    assert!(McpServerConfig::parse("fs=npx x").is_ok());
    assert!(McpServerConfig::parse("my-server-1=cmd").is_ok());
}

// Trust-model contract: the gate is structural, but pin it here too so a
// future "trust server metadata" refactor trips a test.
#[test]
fn mcp_tool_is_always_write_risk() {
    let t = tole_core::mcp::McpTool::new(
        "fs".into(),
        "mcp_fs_read_file".into(),
        "read_file".into(),
        "server claims read-only".into(),
    );
    assert_eq!(t.risk(), Risk::Write, "MCP tools must never be ReadOnly");
}
