//! MCP client tests (issue #74). The REAL stdio integration is exercised
//! live with a reference server (see PR notes); these unit tests pin the
//! config-parsing and trust-model contracts that don't need a server.

use tole_core::mcp::{token_env_name, McpServerConfig, McpTransport};
use tole_core::tool::{Risk, Tool};

#[test]
fn parses_name_command_args() {
    let cfg =
        McpServerConfig::parse("fs=npx -y @modelcontextprotocol/server-filesystem /tmp").unwrap();
    assert_eq!(cfg.name, "fs");
    assert_eq!(
        cfg.transport,
        McpTransport::Stdio {
            command: "npx".into(),
            args: vec![
                "-y".into(),
                "@modelcontextprotocol/server-filesystem".into(),
                "/tmp".into()
            ],
        }
    );
}

#[test]
fn parses_http_urls_as_the_http_transport() {
    let cfg = McpServerConfig::parse("web=https://mcp.example.com/mcp?x=1").unwrap();
    assert_eq!(cfg.name, "web");
    assert_eq!(
        cfg.transport,
        McpTransport::Http {
            url: "https://mcp.example.com/mcp?x=1".into()
        }
    );
    // Scheme is case-insensitive; a port is fine.
    assert!(McpServerConfig::parse("web=HTTPS://mcp.example.com:8443/mcp").is_ok());
}

#[test]
fn plain_http_is_only_for_loopback() {
    for ok in [
        "a=http://localhost/mcp",
        "a=http://localhost:7801/mcp",
        "a=http://127.0.0.1:7801/mcp",
        "a=http://127.1.2.3/mcp",
        "a=http://[::1]:7801/mcp",
    ] {
        assert!(McpServerConfig::parse(ok).is_ok(), "{ok}");
    }
    for bad in [
        "a=http://example.com/mcp",
        "a=http://10.0.0.5/mcp",
        // Lookalikes: the host is not loopback even though it starts like one.
        "a=http://127.0.0.1.evil.com/mcp",
        "a=http://localhost.evil.com/mcp",
    ] {
        let e = McpServerConfig::parse(bad).unwrap_err();
        assert!(e.contains("loopback"), "{bad}: {e}");
    }
}

#[test]
fn http_specs_refuse_credentials_extra_args_and_bad_hosts() {
    for bad in [
        "a=https://user:pw@example.com/mcp",
        "a=https://example.com@evil.com/mcp",
        "a=http://localhost:80@evil.com/mcp",
        "a=https:///mcp",
        "a=https://example.com:notaport/mcp",
        "a=https://[::1/mcp",
    ] {
        assert!(McpServerConfig::parse(bad).is_err(), "{bad}");
    }
    // The token is never part of the spec.
    let e = McpServerConfig::parse("a=https://example.com/mcp --token x").unwrap_err();
    assert!(e.contains("TOLE_MCP_A_TOKEN"), "{e}");
}

#[test]
fn token_env_name_is_upper_snake_per_server() {
    assert_eq!(token_env_name("nginjen"), "TOLE_MCP_NGINJEN_TOKEN");
    assert_eq!(token_env_name("uteke-cloud"), "TOLE_MCP_UTEKE_CLOUD_TOKEN");
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

#[test]
fn http_specs_the_url_parser_refuses_are_config_errors() {
    // Each of these passes a hand-rolled host/port check but not the url
    // parser the HTTP client uses (port above 65535, a numeric last label
    // that is not an IPv4 address, an encoded host).
    for bad in [
        "a=https://example.com:99999/mcp",
        "a=https://1.2.3.4.5/mcp",
        "a=https://ex%41mple.com/mcp",
        "a=https://exa mple.com/mcp",
    ] {
        assert!(McpServerConfig::parse(bad).is_err(), "{bad}");
    }
}
