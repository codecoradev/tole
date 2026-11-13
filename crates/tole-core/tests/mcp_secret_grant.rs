//! #679: the `secrets` a plugin-lock approval records are the server's real
//! grant. A stdio MCP server's process receives a secret only when its grant
//! names it AND the `[secrets]` release binds it to `mcp`. One test in its
//! own process: it sets a variable in the process environment.

use tole_core::mcp::{register_server_tools, McpServerConfig};
use tole_core::plugin_lock::ManifestTool;
use tole_core::secrets::SecretSet;

const VALUE: &str = "gh-grant-test-0123456789abcdef";

/// A wrapper that records its environment, then runs the mock server.
fn server(dir: &std::path::Path, name: &str, grant: Option<Vec<String>>) -> McpServerConfig {
    use std::os::unix::fs::PermissionsExt;
    let mock = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/mock_mcp.py");
    let wrapper = dir.join(format!("{name}.sh"));
    std::fs::write(
        &wrapper,
        format!(
            "#!/bin/sh\nprintenv > {}\nexec python3 {mock} full\n",
            dir.join(format!("{name}.env")).display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&wrapper, std::fs::Permissions::from_mode(0o755)).unwrap();
    let mut cfg = McpServerConfig::parse(&format!("{name}={}", wrapper.display())).unwrap();
    cfg.secret_grant = grant;
    cfg
}

fn env_seen_by(dir: &std::path::Path, cfg: &McpServerConfig) -> String {
    let accept = |_: &str, _: &str, _: &[ManifestTool]| Ok(());
    let mut reg = tole_core::tool::ToolRegistry::new();
    register_server_tools(&mut reg, cfg, &SecretSet::default(), &accept);
    std::fs::read_to_string(dir.join(format!("{}.env", cfg.name))).unwrap_or_default()
}

#[test]
fn a_server_receives_only_the_secrets_its_approval_granted() {
    // SAFETY (edition 2021: safe call): the only test in this binary.
    std::env::set_var("GITHUB_TOKEN", VALUE);
    let dir = std::env::temp_dir().join(format!("tole-mcp-grant-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();

    // The default release binds GITHUB_TOKEN to `mcp`.
    let none = server(&dir, "nogrant", Some(Vec::new()));
    let granted = server(&dir, "granted", Some(vec!["GITHUB_TOKEN".into()]));
    let unlocked = server(&dir, "embedder", None);

    let seen = env_seen_by(&dir, &none);
    assert!(!seen.is_empty(), "the server ran");
    assert!(!seen.contains(VALUE), "an empty grant passes no secret");
    assert!(
        env_seen_by(&dir, &granted).contains(VALUE),
        "a granted secret passes"
    );
    assert!(
        env_seen_by(&dir, &unlocked).contains(VALUE),
        "without a lock grant, the release alone decides (embedders)"
    );
    let _ = std::fs::remove_dir_all(&dir);
}
