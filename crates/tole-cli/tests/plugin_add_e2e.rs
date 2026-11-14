//! #600 end to end with the REAL binary: a third-party server nobody
//! approved is never started by `plugin list`, `plugin approve` (no
//! sandbox) or `plugin add` (no terminal, or under a session), and none of
//! them writes anything. The "server" is a script that only records that
//! it ran.

#![cfg(feature = "mcp")]
#![cfg(unix)]

use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

fn tmp(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("tole-plugin-add-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(d.join("home/tole")).unwrap();
    std::fs::create_dir_all(d.join("ws")).unwrap();
    d
}

fn spy(dir: &Path) -> String {
    use std::os::unix::fs::PermissionsExt;
    let script = dir.join("spy.sh");
    std::fs::write(
        &script,
        format!("#!/bin/sh\ntouch {}/started\nexit 1\n", dir.display()),
    )
    .unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    script.display().to_string()
}

fn tole(dir: &Path, env: &[(&str, &str)], args: &[&str]) -> Output {
    let mut c = Command::new(env!("CARGO_BIN_EXE_tole"));
    c.args(args)
        .current_dir(dir.join("ws"))
        .env("CODECORA_HOME", dir.join("home"))
        .env("TOLE_NO_UPDATE_CHECK", "1")
        .env_remove("TOLE_UNDER_SESSION")
        .env_remove("TOLE_AGENT_DEPTH")
        .stdin(Stdio::null());
    for (k, v) in env {
        c.env(k, v);
    }
    c.output().unwrap()
}

fn text(o: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&o.stdout),
        String::from_utf8_lossy(&o.stderr)
    )
}

fn nothing_written(dir: &Path) {
    let files: Vec<_> = std::fs::read_dir(dir.join("home/tole"))
        .unwrap()
        .flatten()
        .map(|e| e.file_name())
        .collect();
    assert!(files.is_empty(), "wrote {files:?}");
    assert!(!dir.join("ws/.tole").exists());
}

#[test]
fn add_without_a_terminal_starts_nothing_and_writes_nothing() {
    let d = tmp("tty");
    let spec = format!("spy={}", spy(&d));
    let o = tole(&d, &[], &["plugin", "add", &spec]);
    assert!(!o.status.success());
    assert!(text(&o).contains("interactive only"), "{}", text(&o));
    assert!(!d.join("started").exists(), "the server ran");
    nothing_written(&d);
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn add_under_a_session_is_refused_before_anything_else() {
    for env in [("TOLE_UNDER_SESSION", "1"), ("TOLE_AGENT_DEPTH", "1")] {
        let d = tmp("session");
        let spec = format!("spy={}", spy(&d));
        let o = tole(&d, &[env], &["plugin", "add", &spec]);
        assert!(!o.status.success());
        assert!(text(&o).contains("#684"), "{}", text(&o));
        assert!(!d.join("started").exists());
        nothing_written(&d);
        let _ = std::fs::remove_dir_all(&d);
    }
}

#[test]
fn approve_and_list_never_start_an_unverified_server_without_the_sandbox() {
    let d = tmp("approve");
    let spec = format!("spy={}", spy(&d));
    // `approve` of a server that is not the owner's own, with no sandbox.
    let o = tole(
        &d,
        &[],
        &[
            "plugin",
            "approve",
            "spy",
            "--hash",
            "sha256:00",
            "--mcp-server",
            &spec,
        ],
    );
    assert!(!o.status.success());
    assert!(text(&o).contains("only inside the sandbox"), "{}", text(&o));
    assert!(!d.join("started").exists(), "the server ran");
    nothing_written(&d);
    // `list` shows it unapproved and unverified, and does not start it.
    let o = tole(
        &d,
        &[],
        &["plugin", "list", "--json", "--mcp-server", &spec],
    );
    assert!(o.status.success(), "{}", text(&o));
    let v: serde_json::Value = serde_json::from_slice(&o.stdout).unwrap();
    let row = v["servers"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["name"] == "spy")
        .unwrap_or_else(|| panic!("no spy row: {v}"));
    assert_eq!(row["state"], "unapproved");
    assert_eq!(row["tier"], "third-party-unverified");
    assert!(!d.join("started").exists(), "list started the server");
    let _ = std::fs::remove_dir_all(&d);
}
