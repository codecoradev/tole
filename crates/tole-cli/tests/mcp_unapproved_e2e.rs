//! #680: an MCP server the plugin lock does not approve, or has revoked, is
//! not even started: a stdio server is code, and a refused server must not
//! reach the model through resources or prompts either (the core refuses
//! those for any server its admission check did not accept).

#![cfg(feature = "mcp")]

use std::path::Path;
use std::process::{Command, Stdio};

fn tmp(tag: &str) -> std::path::PathBuf {
    let d = std::env::temp_dir().join(format!("tole-mcp-unseen-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(d.join("home/tole")).unwrap();
    std::fs::create_dir_all(d.join("ws")).unwrap();
    d
}

/// A "server" that only records that it was started.
fn spy(dir: &Path) -> String {
    use std::os::unix::fs::PermissionsExt;
    let script = dir.join("spy.sh");
    std::fs::write(
        &script,
        format!("#!/bin/sh\ntouch {}/started\nexit 1\n", dir.display()),
    )
    .unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    format!("spy={}", script.display())
}

fn run(dir: &Path, spec: &str) -> String {
    let out = Command::new(env!("CARGO_BIN_EXE_tole"))
        .args(["run", "--mcp-server", spec, "hi"])
        .current_dir(dir.join("ws"))
        .env("CODECORA_HOME", dir.join("home"))
        .env("TOLE_BASE_URL", "http://127.0.0.1:9/v1")
        .env("TOLE_MODEL", "m")
        .env("TOLE_API_KEY", "k")
        .env("TOLE_NO_UPDATE_CHECK", "1")
        .stdin(Stdio::null())
        .output()
        .expect("run tole");
    String::from_utf8_lossy(&out.stderr).into_owned()
}

#[test]
fn an_unapproved_server_is_never_started() {
    let d = tmp("unlocked");
    let spec = spy(&d);
    let err = run(&d, &spec);
    assert!(err.contains("mcp[spy]: not approved"), "{err}");
    assert!(!d.join("started").exists(), "the server process ran");
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn a_revoked_server_is_never_started() {
    let d = tmp("revoked");
    let spec = spy(&d);
    let mut lock = tole_core::plugin_lock::Lockfile::default();
    lock.servers.insert(
        "spy".into(),
        tole_core::plugin_lock::LockEntry {
            source: format!("stdio:{}", d.join("spy.sh").display()),
            manifest_hash: tole_core::plugin_lock::manifest_hash(&[]),
            revoked: true,
            ..Default::default()
        },
    );
    lock.save(&d.join("home/tole").join(tole_core::plugin_lock::LOCK_FILE))
        .unwrap();
    let err = run(&d, &spec);
    assert!(err.contains("mcp[spy]: revoked"), "{err}");
    assert!(!d.join("started").exists(), "the server process ran");
    let _ = std::fs::remove_dir_all(&d);
}
