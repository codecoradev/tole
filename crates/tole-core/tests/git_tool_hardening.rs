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
        // No process-wide set_var (the tests run on several threads): the
        // host git of the fixture gets its environment here.
        .env("HOME", dir)
        .env("GIT_CONFIG_NOSYSTEM", "1")
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

/// What a sandboxed command with a writable workspace and no repository can do:
/// `git init` there, plant a hook, a clean filter, a `gpg.program` and a
/// `.gitattributes`, so that the HOST-side tool would run them. Returns the
/// marker files each of them would create.
fn plant_a_repository(ws: &Path) -> Vec<PathBuf> {
    let root = ws.parent().unwrap();
    let markers: Vec<PathBuf> = ["hook", "filter", "gpg"]
        .iter()
        .map(|n| root.join(format!("marker-planted-{n}")))
        .collect();
    host_git(ws, &["init", "-q"]);
    host_git(ws, &["config", "user.name", "t"]);
    host_git(ws, &["config", "user.email", "t@t"]);
    script(
        &ws.join(".git/hooks/pre-commit"),
        &format!("touch {}", markers[0].display()),
    );
    script(
        &ws.join("clean.sh"),
        &format!("touch {}\ncat", markers[1].display()),
    );
    host_git(
        ws,
        &[
            "config",
            "filter.x.clean",
            ws.join("clean.sh").to_str().unwrap(),
        ],
    );
    std::fs::write(ws.join(".gitattributes"), "* filter=x\n").unwrap();
    script(
        &ws.join("gpg.sh"),
        &format!("touch {}\nexit 1", markers[2].display()),
    );
    host_git(
        ws,
        &["config", "gpg.program", ws.join("gpg.sh").to_str().unwrap()],
    );
    host_git(ws, &["config", "commit.gpgsign", "true"]);
    markers
}

fn consumers_on() {
    command::set_consumers(Consumers {
        run_command: Mode::Required,
        ..Consumers::default()
    })
    .unwrap();
}

/// The gap: no repository at the workspace root when the session starts, then a
/// sandboxed command makes one. The tool (built at start-up) must refuse every
/// op with a clear reason, and nothing the planted repository names may run.
#[test]
fn a_repository_created_after_start_up_is_not_trusted_and_nothing_fires() {
    consumers_on();
    let dir = TestDir::new("tole-gittool-planted");
    let ws = std::fs::canonicalize(&*dir).unwrap().join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    // Start-up: the tool is built while there is no repository.
    let tool = GitTool::new().in_dir(&ws);
    // A sandboxed-style step plants one.
    std::fs::write(ws.join("f.txt"), "one\n").unwrap();
    let markers = plant_a_repository(&ws);
    for input in [
        json!({"op": "status"}),
        json!({"op": "diff"}),
        json!({"op": "add", "paths": ["f.txt"]}),
        json!({"op": "commit", "message": "x"}),
    ] {
        let e = tool.execute(input.clone()).unwrap_err();
        assert!(
            e.contains("git: refused: no git repository was present when the session started")
                && e.contains("restart the session"),
            "{input}: {e}"
        );
    }
    for m in &markers {
        assert!(!m.exists(), "{} fired on the host", m.display());
    }
    // Nothing was staged or committed either.
    let log = Command::new("git")
        .args(["log", "--oneline"])
        .current_dir(&ws)
        .output()
        .unwrap();
    assert!(!log.status.success(), "no commit exists");
}

/// The bare variant: the workdir is itself a bare repository (HEAD, objects,
/// refs, config, no `.git`) while an enclosing repository existed at start-up:
/// git would use the bare one, so the tool refuses.
#[test]
fn a_workdir_that_is_a_bare_repository_is_refused() {
    consumers_on();
    let dir = TestDir::new("tole-gittool-bare");
    let root = std::fs::canonicalize(&*dir).unwrap();
    host_git(&root, &["init", "-q", "-b", "main"]);
    let bare = root.join("sub");
    std::fs::create_dir_all(bare.join("objects")).unwrap();
    std::fs::create_dir_all(bare.join("refs")).unwrap();
    std::fs::write(bare.join("HEAD"), "ref: refs/heads/main\n").unwrap();
    std::fs::write(bare.join("config"), "[core]\n\tbare = true\n").unwrap();
    let tool = GitTool::new().in_dir(&bare);
    for input in [
        json!({"op": "status"}),
        json!({"op": "diff"}),
        json!({"op": "add", "paths": ["f.txt"]}),
        json!({"op": "commit", "message": "x"}),
    ] {
        let e = tool.execute(input.clone()).unwrap_err();
        assert!(e.contains("looks like a bare repository"), "{input}: {e}");
    }
}

/// A `.git` that was a real directory at start-up and is later swapped for a
/// link (or something else) is refused per call.
#[test]
fn a_git_directory_swapped_after_start_up_is_refused() {
    consumers_on();
    let dir = TestDir::new("tole-gittool-swap");
    let root = std::fs::canonicalize(&*dir).unwrap();
    let ws = root.join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    host_git(&ws, &["init", "-q"]);
    let tool = GitTool::new().in_dir(&ws);
    // Works while it is the same real directory.
    tool.execute(json!({"op": "status"})).unwrap();
    // Swapped for a link to another repository.
    let other = root.join("other");
    std::fs::create_dir_all(&other).unwrap();
    host_git(&other, &["init", "-q"]);
    std::fs::rename(ws.join(".git"), root.join("moved-git")).unwrap();
    std::os::unix::fs::symlink(other.join(".git"), ws.join(".git")).unwrap();
    let e = tool.execute(json!({"op": "status"})).unwrap_err();
    assert!(
        e.contains("not the one that was there when the session started"),
        "{e}"
    );
}
