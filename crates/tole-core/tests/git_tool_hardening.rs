//! #748: the `git` tool runs on the HOST, outside the command sandbox, where
//! `.git` is read-only for a sandboxed command, so it is how a model commits.
//! With a command consumer on, what the repository config and hooks name must
//! not run through it unless it is the owner's own installed hook:
//!
//! * a `core.hooksPath` that points at scripts inside the writable workspace
//!   (husky style) does not fire; the owner's `.git/hooks/pre-commit` still
//!   does, and the commit succeeds;
//! * a `core.fsmonitor` command never runs (always, consumer or not).
//!
//! Host-side proof: marker files the scripts would create. No bubblewrap is
//! needed, the tool is a host process. The consumer settings are process-wide
//! and set once here (`git_tool_default.rs` is the no-consumer side).

#![cfg(all(unix, feature = "shell-tools"))]

#[path = "support/test_dir.rs"]
mod test_dir;
use serde_json::json;
use std::path::{Path, PathBuf};
use std::process::Command;
use test_dir::TestDir;
use tole_core::integrations::git::GitTool;
use tole_core::sandbox::command::{self, Consumers};
use tole_core::sandbox::Mode;
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

struct Repo {
    _dir: TestDir,
    ws: PathBuf,
    evil_hook: PathBuf,
    evil_fsmonitor: PathBuf,
    owner_hook: PathBuf,
}

/// A repository whose config (the OWNER's, husky style) points `core.hooksPath`
/// at `evilhooks/` inside the workspace and `core.fsmonitor` at a script there,
/// with the owner's own `.git/hooks/pre-commit` installed.
fn repo() -> Repo {
    let dir = TestDir::new("tole-gittool-hard");
    let ws = std::fs::canonicalize(&*dir).unwrap().join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    let home = ws.parent().unwrap().join("home");
    std::fs::create_dir_all(&home).unwrap();
    std::env::set_var("HOME", &home);
    std::env::set_var("GIT_CONFIG_NOSYSTEM", "1");
    host_git(&ws, &["init", "-q"]);
    host_git(&ws, &["config", "user.name", "t"]);
    host_git(&ws, &["config", "user.email", "t@t"]);
    let root = ws.parent().unwrap();
    let (evil_hook, evil_fsmonitor, owner_hook) = (
        root.join("marker-evil-hook"),
        root.join("marker-evil-fsmonitor"),
        root.join("marker-owner-hook"),
    );
    script(
        &ws.join("evilhooks/pre-commit"),
        &format!("touch {}", evil_hook.display()),
    );
    script(
        &ws.join("evil-fsmonitor.sh"),
        &format!("touch {}\nexit 1", evil_fsmonitor.display()),
    );
    script(
        &ws.join(".git/hooks/pre-commit"),
        &format!("touch {}", owner_hook.display()),
    );
    host_git(&ws, &["config", "core.hooksPath", "evilhooks"]);
    host_git(
        &ws,
        &[
            "config",
            "core.fsmonitor",
            ws.join("evil-fsmonitor.sh").to_str().unwrap(),
        ],
    );
    Repo {
        _dir: dir,
        ws,
        evil_hook,
        evil_fsmonitor,
        owner_hook,
    }
}

#[test]
fn with_a_command_consumer_on_only_the_owners_installed_hooks_run_and_no_fsmonitor() {
    command::set_consumers(Consumers {
        run_command: Mode::Required,
        ..Consumers::default()
    })
    .unwrap();
    let r = repo();
    let tool = GitTool::new().in_dir(&r.ws);
    std::fs::write(r.ws.join("f.txt"), "one\n").unwrap();
    tool.execute(json!({"op": "status"})).unwrap();
    tool.execute(json!({"op": "add", "paths": ["f.txt"]}))
        .unwrap();
    tool.execute(json!({"op": "diff", "staged": true})).unwrap();
    tool.execute(json!({"op": "commit", "message": "add f"}))
        .expect("the commit succeeds");
    // The commit really happened.
    let log = Command::new("git")
        .args(["log", "--oneline"])
        .current_dir(&r.ws)
        .output()
        .unwrap();
    assert!(String::from_utf8_lossy(&log.stdout).contains("add f"));
    // The owner's own hook ran (pinned `.git/hooks`)...
    assert!(
        r.owner_hook.exists(),
        "the owner's pre-commit hook must run"
    );
    // ...the in-workspace hooksPath script and the fsmonitor did not.
    assert!(
        !r.evil_hook.exists(),
        "a core.hooksPath script inside the workspace must not run on the host"
    );
    assert!(
        !r.evil_fsmonitor.exists(),
        "a core.fsmonitor command must not run on the host"
    );
}

/// The model is told where the way out is: `run_command`'s summary (its
/// consumer is on here) names the `git` tool; `job_start`'s consumer is off, so
/// its summary does not.
#[test]
fn the_run_command_summary_points_at_the_git_tool_when_its_consumer_is_on() {
    command::set_consumers(Consumers {
        run_command: Mode::Required,
        ..Consumers::default()
    })
    .unwrap();
    let w = std::env::temp_dir();
    let s = tole_core::run_command::RunCommandTool::new(w.clone()).summary();
    assert!(
        s.contains(".git directory is read-only") && s.contains("use the git tool"),
        "{s}"
    );
    let j = tole_core::jobs::JobStartTool::new(w).summary();
    assert!(!j.contains("git tool"), "{j}");
}
