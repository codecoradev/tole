//! Shared helpers of the sandbox lifecycle tests (#601): a policy with
//! chosen limits, a registered server per workspace, a host-side check that
//! its process is alive.

#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use tole_core::mcp::{register_server_tools, McpServerConfig};
use tole_core::plugin_lock::ManifestTool;
use tole_core::sandbox::{self, Availability, Mode, Policy};
use tole_core::secrets::SecretSet;
use tole_core::tool::ToolRegistry;

/// Sets the process policy: required, profile `server`, these limits.
pub fn live_policy(max_servers: usize, idle_secs: u64) {
    sandbox::set_policy(Some(Policy {
        mode: Mode::Required,
        profile_name: "server".into(),
        profile: sandbox::builtin_profile("server").unwrap(),
        skip: Vec::new(),
        max_servers,
        idle_secs,
    }))
    .unwrap();
}

/// True (after printing the reason) when no sandbox works here.
pub fn skipped() -> bool {
    match sandbox::detect() {
        Availability::Usable { .. } => false,
        Availability::Unavailable(why) => {
            assert!(
                std::env::var("TOLE_REQUIRE_BWRAP").is_err(),
                "TOLE_REQUIRE_BWRAP is set but no sandbox works: {why}"
            );
            eprintln!("SKIPPED: no working sandbox here: {why}");
            true
        }
    }
}

/// A fresh workspace directory holding the mock server.
pub fn workspace(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("tole-sbx-life-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    let d = std::fs::canonicalize(&d).unwrap();
    std::fs::copy(
        concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/mock_mcp.py"),
        d.join("mock_mcp.py"),
    )
    .unwrap();
    d
}

/// Registers the mock server `name` for the session whose workspace is `ws`.
/// The returned registry holds the session's claim: drop it to end the
/// session.
pub fn register(name: &str, ws: &Path) -> (McpServerConfig, ToolRegistry) {
    let mut cfg = McpServerConfig::parse(&format!(
        "{name}=python3 {}",
        ws.join("mock_mcp.py").display()
    ))
    .unwrap();
    cfg.secret_grant = Some(Vec::new());
    cfg.workspace = Some(ws.to_path_buf());
    let accept = |_: &str, _: &str, _: &[ManifestTool]| Ok(());
    let mut reg = ToolRegistry::new();
    register_server_tools(&mut reg, &cfg, &SecretSet::default(), &accept);
    (cfg, reg)
}

/// Whether a (non-zombie) process runs the mock server of `ws`, seen from
/// the host: a zombie has an empty command line.
pub fn running(ws: &Path) -> bool {
    let needle = ws.join("mock_mcp.py").display().to_string();
    let Ok(dir) = std::fs::read_dir("/proc") else {
        return false;
    };
    dir.flatten().any(|e| {
        e.file_name()
            .to_string_lossy()
            .chars()
            .all(|c| c.is_ascii_digit())
            && std::fs::read(e.path().join("cmdline"))
                .map(|b| String::from_utf8_lossy(&b).contains(&needle))
                .unwrap_or(false)
    })
}

/// Polls `f` for up to ten seconds.
pub fn wait_until(f: impl Fn() -> bool) -> bool {
    let end = Instant::now() + Duration::from_secs(10);
    while Instant::now() < end {
        if f() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    f()
}
