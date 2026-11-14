//! #684 end-to-end with the REAL binary: the owner-only subcommands
//! (`cron create|resume|edit`, `plugin approve`, `skills approve`,
//! `config trust`) refuse to write under a session (`TOLE_UNDER_SESSION`,
//! or a child agent's `TOLE_AGENT_DEPTH`), and a human at a terminal keeps
//! working. `CODECORA_HOME` is set on the CHILD only.

#![cfg(feature = "shell-tools")]

use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};

use serde_json::json;
use tole_core::run_command::RunCommandTool;
use tole_core::tool::Tool;

static COUNTER: AtomicU64 = AtomicU64::new(0);

fn tmpdir() -> PathBuf {
    let n = COUNTER.fetch_add(1, Ordering::SeqCst);
    let d = std::env::temp_dir().join(format!("tole-owner-684-{}-{n}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d.canonicalize().unwrap()
}

/// `tole` with `extra` env on top of a clean session-free environment.
fn tole(bin: &Path, cwd: &Path, home: &Path, extra: &[(&str, &str)], args: &[&str]) -> Output {
    let mut cmd = Command::new(bin);
    cmd.args(args)
        .current_dir(cwd)
        .env("TOLE_NO_UPDATE_CHECK", "1")
        .env("CODECORA_HOME", home)
        .env_remove("TOLE_UNDER_SESSION")
        .env_remove("TOLE_AGENT_DEPTH")
        .stdin(Stdio::null());
    for (k, v) in extra {
        cmd.env(k, v);
    }
    cmd.output().unwrap()
}

fn real() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_tole"))
}

fn text(o: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&o.stdout),
        String::from_utf8_lossy(&o.stderr)
    )
}

/// Files under the data dir (relative names); the cron ledger, plugin
/// lock, approvals and trust store all live there.
fn data_files(home: &Path) -> Vec<String> {
    fn walk(d: &Path, base: &Path, out: &mut Vec<String>) {
        let Ok(rd) = std::fs::read_dir(d) else { return };
        for e in rd.flatten() {
            let p = e.path();
            if p.is_dir() {
                walk(&p, base, out);
            } else {
                out.push(p.strip_prefix(base).unwrap().display().to_string());
            }
        }
    }
    let mut out = Vec::new();
    walk(home, home, &mut out);
    out.sort();
    out
}

fn project(home_for_cfg: &str) -> PathBuf {
    let d = tmpdir();
    std::fs::create_dir_all(d.join(".tole")).unwrap();
    std::fs::write(d.join(".tole/config.toml"), home_for_cfg).unwrap();
    d
}

const SESSION: &[(&str, &str)] = &[("TOLE_UNDER_SESSION", "1")];
const CHILD: &[(&str, &str)] = &[("TOLE_AGENT_DEPTH", "1")];

fn owner_commands() -> Vec<Vec<&'static str>> {
    let mut v = vec![
        vec!["cron", "create", "@every 10m", "--script", "echo pwned"],
        vec!["cron", "resume", "job-x"],
        vec!["cron", "edit", "job-x", "--script", "echo pwned"],
        vec!["skills", "approve", "evil", "--hash", "sha256:00"],
        vec!["skills", "drafts", "promote", "d-1", "--hash", "sha256:00"],
        vec!["config", "trust", "--yes"],
    ];
    #[cfg(feature = "mcp")]
    v.push(vec!["plugin", "approve", "evil", "--hash", "sha256:00"]);
    v
}

#[test]
fn owner_only_subcommands_refuse_under_a_session_and_write_nothing() {
    for env in [SESSION, CHILD] {
        let home = tmpdir();
        let cwd = project("model = \"m1\"\n");
        // Only `config trust` reads the project config, and every other
        // command is gated on the cwd's config being trusted.
        let bare = tmpdir();
        for args in owner_commands() {
            let at = if args[0] == "config" { &cwd } else { &bare };
            let o = tole(&real(), at, &home, env, &args);
            assert!(!o.status.success(), "{args:?} must fail under {env:?}");
            assert!(text(&o).contains("#684"), "{args:?}: {}", text(&o));
        }
        assert_eq!(data_files(&home), Vec::<String>::new(), "nothing written");
    }
}

#[test]
fn a_human_at_a_terminal_keeps_working() {
    let home = tmpdir();
    let cwd = project("model = \"m1\"\n");
    let o = tole(
        &real(),
        &cwd,
        &home,
        &[],
        &["cron", "create", "@every 10m", "--script", "echo hi"],
    );
    assert!(o.status.success(), "{}", text(&o));
    let o = tole(&real(), &cwd, &home, &[], &["config", "trust", "--yes"]);
    assert!(o.status.success(), "{}", text(&o));
    assert!(!data_files(&home).is_empty());
}

/// The scenario of the issue: a session runs the CLI through run_command.
/// The tole binary by name is refused; a renamed copy (the documented
/// residual of the name check) still carries the marker the subcommands
/// refuse on, so it writes nothing either.
#[cfg(unix)]
#[test]
fn run_command_cannot_reach_the_owner_only_records() {
    let home = tmpdir();
    let work = tmpdir();
    let copy = work.join("helper");
    std::fs::copy(real(), &copy).unwrap();
    let t = RunCommandTool::new(work.clone());
    let envs = format!("CODECORA_HOME={}", home.display());

    // By name (and behind a wrapper): refused before any spawn.
    for line in [
        "tole cron create '@every 10m' --script 'echo pwned'",
        &format!("env {envs} tole cron resume job-x"),
    ] {
        let err = t.execute(json!({ "command": line })).unwrap_err();
        assert!(err.contains("#684"), "{line}: {err}");
    }

    // Renamed copy, marker inherited from the tool: the CLI refuses.
    let line = format!(
        "env {envs} TOLE_NO_UPDATE_CHECK=1 {} cron create '@every 10m' --script 'echo pwned'",
        copy.display()
    );
    let out = t.execute(json!({ "command": line })).unwrap();
    assert_ne!(out["status"], 0, "{out}");
    assert!(out["stderr"].as_str().unwrap().contains("#684"), "{out}");
    assert_eq!(data_files(&home), Vec::<String>::new(), "nothing written");
}
