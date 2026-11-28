//! #1286 review: what runs is judged, not what is written. A `#!` line is read
//! as the kernel reads it (up to a NUL), every interpreter on the chain is
//! judged and the chain is bounded, a program and an interpreter are judged
//! under every name of their links, the running binary is refused by inode
//! (a hard link too), and a file that is not a regular file is never opened
//! for reading in a way that blocks.
#![cfg(all(unix, feature = "shell-tools"))]

#[path = "support/test_dir.rs"]
mod test_dir;
use std::path::Path;
use test_dir::{write_executable, TestDir};
use tole_core::program_path::check_command_argv;

fn check(ws: &Path, line: &str) -> Result<(), String> {
    let argv: Vec<String> = line.split(' ').map(str::to_string).collect();
    check_command_argv(&argv, ws)
}

fn refused(ws: &Path, line: &str) -> String {
    check(ws, line).expect_err(&format!("must refuse: {line}"))
}

/// A `tole` that a session must not run, as a plain script.
fn plant_tole(ws: &Path) {
    std::fs::create_dir_all(ws.join("bin")).unwrap();
    write_executable(ws.join("bin/tole"), "#!/bin/sh\necho owner-only\n");
}

#[test]
fn a_nul_ends_the_interpreter_as_it_does_in_the_kernel() {
    let ws = TestDir::new("tole-pqs-nul");
    plant_tole(&ws);
    write_executable(ws.join("nul"), b"#!./bin/tole\0junk\n".as_slice());
    write_executable(ws.join("nul2"), b"#!/usr/bin/env\0 sh\n".as_slice());
    let err = refused(&ws, "./nul");
    assert!(err.contains("tole binary"), "{err}");
    // And the word after the NUL is not read as the interpreter's argument.
    assert!(check(&ws, "./nul2").is_ok());
}

#[test]
fn every_interpreter_on_the_chain_is_judged_and_the_chain_is_bounded() {
    let ws = TestDir::new("tole-pqs-chain");
    plant_tole(&ws);
    write_executable(ws.join("y"), "#!./bin/tole\n");
    write_executable(ws.join("x"), "#!./y\n");
    let err = refused(&ws, "./x");
    assert!(
        err.contains("`#!` line") && err.contains("tole binary"),
        "{err}"
    );
    // A chain of plain interpreters that ends in a program is fine ...
    write_executable(ws.join("c0"), "#!/bin/sh\n");
    for i in 1..=3 {
        write_executable(ws.join(format!("c{i}")), format!("#!./c{}\n", i - 1));
    }
    assert!(
        check(&ws, "./c3").is_ok(),
        "four levels is the kernel's limit"
    );
    // ... but deeper than the kernel follows is refused, not guessed.
    write_executable(ws.join("c4"), "#!./c3\n");
    write_executable(ws.join("c5"), "#!./c4\n");
    let err = refused(&ws, "./c5");
    assert!(err.contains("deeper than the kernel follows"), "{err}");
    // An interpreter that is not there, or none at all.
    write_executable(ws.join("gone"), "#!./nothing\n");
    write_executable(ws.join("empty"), "#!\n");
    for p in ["./gone", "./empty"] {
        assert!(check(&ws, p).is_err(), "{p}");
    }
}

#[test]
fn the_interpreter_is_judged_under_every_name_of_its_links() {
    let ws = TestDir::new("tole-pqs-links");
    plant_tole(&ws);
    std::os::unix::fs::symlink(ws.join("bin/tole"), ws.join("lnk")).unwrap();
    write_executable(ws.join("a"), format!("#!{}/lnk\n", ws.display()));
    let err = refused(&ws, "./a");
    assert!(err.contains("tole binary"), "{err}");
    // A teardown program behind an innocent link name.
    write_executable(ws.join("shutdown"), "#!/bin/sh\n");
    std::os::unix::fs::symlink(ws.join("shutdown"), ws.join("ok")).unwrap();
    write_executable(ws.join("b"), format!("#!{}/ok\n", ws.display()));
    assert!(refused(&ws, "./b").contains("destructive"));
}

#[test]
fn a_link_to_a_shell_is_a_shell() {
    let ws = TestDir::new("tole-pqs-shell");
    plant_tole(&ws);
    // `/bin/sh` is often a link to dash: the hops ("sh") are judged, not only
    // the file it ends at.
    std::os::unix::fs::symlink("/bin/sh", ws.join("shl")).unwrap();
    for payload in ["tole x", "rm -rf /nonexistent"] {
        let argv = ["./shl", "-c", payload].map(str::to_string);
        let err = check_command_argv(&argv, &ws).expect_err(payload);
        assert!(
            err.contains("tole binary") || err.contains("destructive"),
            "{payload}: {err}"
        );
    }
    assert!(check_command_argv(&["./shl".into(), "-c".into(), "echo hi".into()], &ws).is_ok());
}

#[test]
fn the_running_binary_is_refused_by_inode_under_any_name() {
    let ws = TestDir::new("tole-pqs-inode");
    let me = std::env::current_exe().unwrap();
    if std::fs::hard_link(&me, ws.join("hl")).is_err() {
        return; // another file system: no hard link to make
    }
    assert!(refused(&ws, "./hl x").contains("tole binary"));
    write_executable(ws.join("via"), format!("#!{}/hl\n", ws.display()));
    assert!(refused(&ws, "./via").contains("tole binary"));
}

#[test]
fn a_pipe_is_refused_and_never_waited_on() {
    let ws = TestDir::new("tole-pqs-fifo");
    let made = std::process::Command::new("mkfifo")
        .arg(ws.join("pipe"))
        .status();
    if !made.is_ok_and(|s| s.success()) {
        return; // no mkfifo on this host
    }
    std::fs::set_permissions(
        ws.join("pipe"),
        std::os::unix::fs::PermissionsExt::from_mode(0o755),
    )
    .unwrap();
    // As the program, and as the interpreter of a script.
    write_executable(ws.join("viapipe"), format!("#!{}/pipe\n", ws.display()));
    for line in ["./pipe", "./viapipe"] {
        let err = refused(&ws, line);
        assert!(err.contains("no executable file is there"), "{line}: {err}");
    }
}
