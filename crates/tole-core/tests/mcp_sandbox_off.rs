//! #601: with no `[sandbox]` policy (the default) a stdio MCP server starts
//! exactly as before: the host's network, files and `$HOME`, with the ENV-1
//! scrub still applied. One test in its own process: it sets a canary
//! variable and never sets a sandbox policy.

#![cfg(unix)]

use std::os::unix::fs::PermissionsExt;
use tole_core::mcp::{register_server_tools, McpServerConfig};
use tole_core::plugin_lock::ManifestTool;
use tole_core::secrets::SecretSet;

const CANARY: &str = "sandbox-off-canary-not-a-credential-0123456789";

#[test]
fn without_a_policy_the_launch_is_unchanged() {
    assert!(tole_core::sandbox::policy().is_none());
    // The reactor key is the plain name: behaviour unchanged.
    assert_eq!(tole_core::mcp::conn_key_for("off", None), "off");
    let base = std::env::temp_dir().join(format!("tole-sbx-off-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    let (ws, out) = (base.join("ws"), base.join("out"));
    for d in [&ws, &out] {
        std::fs::create_dir_all(d).unwrap();
    }
    std::fs::write(base.join("secret"), "x").unwrap();
    std::env::set_var("GITHUB_TOKEN", CANARY);
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    std::fs::copy(
        concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/mock_mcp.py"),
        ws.join("mock_mcp.py"),
    )
    .unwrap();
    let wrapper = ws.join("srv.sh");
    let o = out.display();
    std::fs::write(
        &wrapper,
        format!(
            "#!/bin/sh\n\
             cd \"$(dirname \"$0\")\"\n\
             printenv > {o}/env\n\
             (test -e {s} && echo yes || echo no) > {o}/data\n\
             echo \"$HOME\" > {o}/home\n\
             python3 -c \"import socket; s=socket.socket(); s.settimeout(2); s.connect(('127.0.0.1',{port}))\" 2>/dev/null && echo reached > {o}/net || echo blocked > {o}/net\n\
             exec python3 {m} full\n",
            s = base.join("secret").display(),
            m = ws.join("mock_mcp.py").display(),
        ),
    )
    .unwrap();
    std::fs::set_permissions(&wrapper, std::fs::Permissions::from_mode(0o755)).unwrap();
    let mut cfg = McpServerConfig::parse(&format!("off={}", wrapper.display())).unwrap();
    cfg.secret_grant = Some(Vec::new());
    let accept = |_: &str, _: &str, _: &[ManifestTool]| Ok(());
    let mut reg = tole_core::tool::ToolRegistry::new();
    register_server_tools(&mut reg, &cfg, &SecretSet::default(), &accept);

    let read = |f: &str| {
        std::fs::read_to_string(out.join(f))
            .unwrap_or_default()
            .trim()
            .to_string()
    };
    assert!(
        tole_core::mcp::probe_server_extras(&cfg).resources,
        "the server answered"
    );
    assert_eq!(read("net"), "reached", "the host network");
    assert_eq!(read("data"), "yes", "host files are visible");
    assert_ne!(read("home"), "/scratch");
    assert!(!read("env").contains(CANARY), "ENV-1 still scrubs");
    let _ = std::fs::remove_dir_all(&base);
}
