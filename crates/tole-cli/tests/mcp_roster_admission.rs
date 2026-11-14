//! #680 at the roster: a server that IS connected (its lock entry exists)
//! but is refused because its manifest changed gets no `mcp_read` entry;
//! the same server, approved as it is, does. One test in its own process:
//! it points the tole data dir (the user plugin lock) at a temp dir.

#![cfg(feature = "mcp")]

use tole_cli::tool_roster::{build, Roster};
use tole_core::approval::{Approver, ToolRequest, Verdict};
use tole_core::mcp::McpServerConfig;
use tole_core::plugin_lock::{manifest_hash, LockEntry, Lockfile, ManifestTool, LOCK_FILE};

struct Deny;
impl Approver for Deny {
    fn decide(&self, _req: &ToolRequest<'_>) -> Verdict {
        Verdict::Deny
    }
}

fn mock() -> McpServerConfig {
    let script = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../tole-core/tests/fixtures/mock_mcp.py"
    );
    McpServerConfig::parse(&format!("res=python3 {script} full")).unwrap()
}

fn lock_with(dir: &std::path::Path, cfg: &McpServerConfig, hash: String, version: &str) {
    let source = tole_cli::plugin_gate::source_of(cfg);
    let mut lock = Lockfile::default();
    lock.servers.insert(
        "res".into(),
        LockEntry {
            integrity: tole_cli::plugin_gate::integrity(&source),
            source,
            version: version.into(),
            manifest_hash: hash,
            ..Default::default()
        },
    );
    lock.save(&dir.join("tole").join(LOCK_FILE)).unwrap();
}

fn has_mcp_read(ws: &std::path::Path, cfg: &McpServerConfig) -> bool {
    let mut roster = Roster::new(ws.to_path_buf());
    roster.mcp_servers = vec![cfg.clone()];
    build(Deny, roster).unwrap().get("mcp_read").is_some()
}

#[test]
fn a_changed_server_gets_no_mcp_read_and_an_approved_one_does() {
    let dir = std::env::temp_dir().join(format!("tole-roster-680-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("tole")).unwrap();
    std::fs::create_dir_all(dir.join("ws")).unwrap();
    // SAFETY (edition 2021: safe call): the only test in this binary.
    std::env::set_var("CODECORA_HOME", &dir);
    let cfg = mock();
    let (version, tools) = tole_core::mcp::list_manifest(&cfg).unwrap();

    // Approved, but for another tool list: the live manifest changed.
    let other = vec![ManifestTool {
        name: "gone".into(),
        description: "a tool the server no longer lists".into(),
        input_schema: serde_json::json!({"type": "object"}),
    }];
    lock_with(&dir, &cfg, manifest_hash(&other), &version);
    assert!(
        !has_mcp_read(&dir.join("ws"), &cfg),
        "a refused server must not be offered through mcp_read"
    );

    // Control: approved exactly as it is, it offers its resources.
    lock_with(&dir, &cfg, manifest_hash(&tools), &version);
    assert!(has_mcp_read(&dir.join("ws"), &cfg));
    let _ = std::fs::remove_dir_all(&dir);
}
