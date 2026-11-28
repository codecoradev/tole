//! #1286: `run_command` and `job_start` run a program written as a path when
//! it is an executable file the blocklist has nothing against, read on the
//! name as written, on the name the file resolves to and on its `#!` line.
//! Everything a bare name is refused for is refused here too, the tole binary
//! under any spelling included. The cron argv rule is untouched
//! (`check_destructive_argv` still refuses every path; see
//! `path_qualified_program.rs`).
#![cfg(all(unix, feature = "shell-tools"))]

#[path = "support/test_dir.rs"]
mod test_dir;
use serde_json::{json, Value};
use test_dir::{write_executable, TestDir};
use tole_core::approval::{pattern_verdict, PatternVerdict, ToolRequest};
use tole_core::jobs::JobStartTool;
use tole_core::run_command::RunCommandTool;
use tole_core::tool::{Risk, Tool};

const DESTRUCTIVE: &str = "destructive beyond the Write risk tier";
const NO_FILE: &str = "no executable file is there";

fn run(ws: &TestDir, line: &str) -> Result<Value, String> {
    RunCommandTool::new(ws.to_path_buf()).execute(json!({ "command": line }))
}

fn refused(ws: &TestDir, line: &str) -> String {
    run(ws, line).expect_err(&format!("must refuse: {line}"))
}

#[test]
fn a_program_in_the_workspace_runs_by_its_relative_path() {
    let ws = TestDir::new("tole-pq-rel");
    std::fs::create_dir_all(ws.join("target/release")).unwrap();
    write_executable(
        ws.join("target/release/mytool"),
        "#!/bin/sh\necho \"ran $1\"\n",
    );
    for line in ["./target/release/mytool list", "target/release/mytool list"] {
        let out = run(&ws, line).unwrap_or_else(|e| panic!("{line}: {e}"));
        assert_eq!(out["status"], 0, "{line}");
        assert_eq!(out["stdout"].as_str().unwrap().trim(), "ran list");
    }
}

#[test]
fn a_system_program_runs_by_its_absolute_path() {
    let ws = TestDir::new("tole-pq-abs");
    let out = run(&ws, "/bin/echo hi").unwrap();
    assert_eq!(out["stdout"].as_str().unwrap().trim(), "hi");
}

#[test]
fn what_a_bare_name_is_refused_for_is_refused_behind_a_path() {
    let ws = TestDir::new("tole-pq-destructive");
    write_executable(ws.join("rm"), "#!/bin/sh\nexit 0\n");
    std::os::unix::fs::symlink("/bin/rm", ws.join("harmless")).unwrap();
    std::fs::create_dir(ws.join("sub")).unwrap();
    std::os::unix::fs::symlink("/bin/echo", ws.join("sub/rm")).unwrap();
    for line in [
        "/bin/rm -rf /",
        "/usr/bin/rm -r ../outside",
        "./rm -rf /etc",
        // A link is what it points to: `rm` under another name.
        "./harmless -rf /",
        "/bin/bash -c 'rm -rf /etc'",
        "/usr/bin/env dd if=/dev/zero of=/dev/sda",
        "/bin/sh -c 'echo hi; shutdown -h now'",
    ] {
        assert!(refused(&ws, line).contains(DESTRUCTIVE), "{line}");
    }
    // A link NAMED rm that points at something harmless is `rm` by name, and
    // scoped `rm` stays allowed like the bare name.
    assert!(refused(&ws, "./sub/rm -rf /").contains(DESTRUCTIVE));
    std::fs::create_dir(ws.join("build")).unwrap();
    assert!(run(&ws, "/bin/rm -rf ./build").is_ok());
}

#[test]
fn a_path_to_nothing_runnable_says_so() {
    let ws = TestDir::new("tole-pq-nofile");
    std::fs::create_dir(ws.join("dir")).unwrap();
    std::fs::write(ws.join("plain.txt"), "x").unwrap(); // no execute bit
    for line in [
        "./missing",
        "./dir",
        "./plain.txt",
        "~/.local/bin/mytool list",
        "/",
    ] {
        let err = refused(&ws, line);
        assert!(err.contains("written as a path"), "{line}: {err}");
        assert!(err.contains(NO_FILE), "{line}: {err}");
        assert!(
            err.contains(&*ws.to_string_lossy()),
            "names the workspace: {err}"
        );
        assert!(!err.contains(DESTRUCTIVE), "{line}: {err}");
    }
}

#[test]
fn the_tole_binary_is_never_let_through_under_a_path() {
    let ws = TestDir::new("tole-pq-tole");
    std::fs::create_dir(ws.join("bin")).unwrap();
    write_executable(ws.join("bin/tole"), "#!/bin/sh\necho owner-only\n");
    write_executable(ws.join("renamed"), "#!/bin/sh\necho owner-only\n");
    std::os::unix::fs::symlink(ws.join("bin/tole"), ws.join("alias")).unwrap();
    write_executable(
        ws.join("script"),
        format!("#!{}/bin/tole cron\n", ws.display()),
    );
    write_executable(ws.join("viaenv"), "#!/usr/bin/env tole\n");
    write_executable(ws.join("tear"), "#!/sbin/shutdown\n");
    for line in [
        "./bin/tole discuss list",
        "/nonexistent/tole discuss list", // not found is not a way round
        "./alias discuss list",           // a link to a file named tole
        "env ./bin/tole x",
        "bash -c './bin/tole x'",
    ] {
        let err = refused(&ws, line);
        assert!(err.contains("tole binary"), "{line}: {err}");
        assert!(!err.contains("use the bare name"), "{line}: {err}");
    }
    for line in ["./script", "./viaenv", "./tear"] {
        let err = refused(&ws, line);
        assert!(err.contains("`#!` line"), "{line}: {err}");
    }
    // A renamed copy is the marker's to stop, not this check's (module text):
    // it runs, and what it runs sees the session marker.
    let out = run(&ws, "./renamed").unwrap();
    assert_eq!(out["stdout"].as_str().unwrap().trim(), "owner-only");
}

#[test]
fn a_program_inside_a_shell_text_stays_refused() {
    let ws = TestDir::new("tole-pq-payload");
    for line in ["sh -c '/bin/echo hi'", "bash -c './x list | head'"] {
        let err = refused(&ws, line);
        assert!(err.contains("written as a path"), "{line}: {err}");
        assert!(err.contains("use the bare name"), "{line}: {err}");
    }
}

#[test]
fn a_bare_name_allow_never_matches_a_path_and_a_deny_still_does() {
    let input = |c: &str| json!({ "command": c });
    let verdict = |patterns: &[&str], c: &str| {
        let i = input(c);
        let req = ToolRequest {
            tool: "run_command",
            risk: Risk::Write,
            input: &i,
            description: String::new(),
            target: None,
            summary: None,
        };
        let p: Vec<String> = patterns.iter().map(|s| s.to_string()).collect();
        pattern_verdict(&p, &req)
    };
    let allow = ["run_command:git status *"];
    assert_eq!(verdict(&allow, "git status -s"), PatternVerdict::Allow);
    assert_eq!(
        verdict(&allow, "/usr/bin/git status -s"),
        PatternVerdict::NoMatch
    );
    assert_eq!(verdict(&allow, "./git status -s"), PatternVerdict::NoMatch);
    assert_eq!(
        verdict(&["!run_command:git push *"], "/usr/bin/git push"),
        PatternVerdict::Deny
    );
}

#[test]
fn job_start_takes_the_same_rule() {
    let ws = TestDir::new("tole-pq-job");
    write_executable(ws.join("job.sh"), "#!/bin/sh\necho done\n");
    write_executable(ws.join("tole"), "#!/bin/sh\n");
    let t = JobStartTool::new(ws.to_path_buf());
    let start = |c: &str| t.execute(json!({ "command": c }));
    assert!(start("./job.sh").is_ok());
    assert!(start("./nope").unwrap_err().contains(NO_FILE));
    assert!(start("/bin/rm -rf /").unwrap_err().contains(DESTRUCTIVE));
    assert!(start("./tole x").unwrap_err().contains("tole binary"));
}
