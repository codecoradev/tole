//! #748, the other side of `git_tool_hardening.rs`: with NO command consumer
//! on (the default), the `git` tool behaves as it always did (the repository's
//! own hooks run, `core.hooksPath` is honoured), except that a `core.fsmonitor`
//! command never runs (`-c core.fsmonitor=false` is always passed).
//!
//! Host-side proof with marker files; the consumer settings are process-wide and
//! left unset here.

#![cfg(all(unix, feature = "shell-tools"))]

#[path = "support/test_dir.rs"]
mod test_dir;
use serde_json::json;
use std::path::Path;
use std::process::Command;
use test_dir::TestDir;
use tole_core::integrations::git::GitTool;
use tole_core::tool::Tool;

fn host_git(dir: &Path, args: &[&str]) {
    let o = Command::new("git")
        .args(args)
        .current_dir(dir)
        .output()
        .expect("git on PATH");
    assert!(o.status.success(), "git {args:?}: {o:?}");
}

fn script(path: &Path, body: &str) {
    use std::os::unix::fs::PermissionsExt;
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, format!("#!/bin/sh\n{body}\n")).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
}

#[test]
fn without_a_consumer_hooks_are_unchanged_and_fsmonitor_is_always_off() {
    let dir = TestDir::new("tole-gittool-default");
    let root = std::fs::canonicalize(&*dir).unwrap();
    let ws = root.join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    std::fs::create_dir_all(root.join("home")).unwrap();
    std::env::set_var("HOME", root.join("home"));
    std::env::set_var("GIT_CONFIG_NOSYSTEM", "1");
    host_git(&ws, &["init", "-q"]);
    host_git(&ws, &["config", "user.name", "t"]);
    host_git(&ws, &["config", "user.email", "t@t"]);
    let (hook, fsm) = (root.join("marker-hook"), root.join("marker-fsmonitor"));
    script(
        &ws.join("myhooks/pre-commit"),
        &format!("touch {}", hook.display()),
    );
    script(
        &ws.join("fsm.sh"),
        &format!("touch {}\nexit 1", fsm.display()),
    );
    host_git(&ws, &["config", "core.hooksPath", "myhooks"]);
    host_git(
        &ws,
        &[
            "config",
            "core.fsmonitor",
            ws.join("fsm.sh").to_str().unwrap(),
        ],
    );
    std::fs::write(ws.join("f.txt"), "one\n").unwrap();
    let tool = GitTool::new().in_dir(&ws);
    tool.execute(json!({"op": "status"})).unwrap();
    tool.execute(json!({"op": "add", "paths": ["f.txt"]}))
        .unwrap();
    tool.execute(json!({"op": "commit", "message": "add f"}))
        .unwrap();
    assert!(
        hook.exists(),
        "no consumer on: the configured hooksPath is honoured as before"
    );
    assert!(
        !fsm.exists(),
        "a core.fsmonitor command never runs through the tool, consumer or not"
    );
}
