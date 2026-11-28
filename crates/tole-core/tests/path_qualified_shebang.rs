//! #1286 review: what runs is judged, not what is written. A `#!` line is read
//! up to a NUL as the kernel does (and split at every blank, which is as strict
//! or stricter), every interpreter on the chain is
//! judged and the chain is bounded, a program and an interpreter are judged
//! under every name of their links, the running binary is refused by inode
//! (a hard link too), and a file that is not a regular file is never opened
//! for reading in a way that blocks.
#![cfg(all(unix, feature = "shell-tools"))]

#[path = "support/test_dir.rs"]
mod test_dir;
#[path = "support/which.rs"]
mod which;
use std::path::Path;
use test_dir::{write_executable, TestDir};
use tole_core::program_path::check_command_argv;
use tole_core::subprocess::check_destructive_argv;

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
    let err = refused(&ws, "./nul");
    assert!(err.contains("tole binary"), "{err}");
    // And the word after the NUL is not read as the interpreter's argument.
    if let Some(env) = which::find_exe("env") {
        let line = format!("#!{}\0 sh\n", env.display());
        write_executable(ws.join("nul2"), line.into_bytes());
        assert!(check(&ws, "./nul2").is_ok());
    }
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
        "the program and four interpreters is the most followed"
    );
    // ... but deeper than that is refused, not guessed.
    write_executable(ws.join("c4"), "#!./c3\n");
    write_executable(ws.join("c5"), "#!./c4\n");
    let err = refused(&ws, "./c5");
    assert!(err.contains("deeper than is followed here"), "{err}");
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
    let Some(sh) = which::find_exe("sh") else {
        return;
    };
    std::os::unix::fs::symlink(sh, ws.join("shl")).unwrap();
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

#[test]
fn an_interpreter_is_judged_with_the_argv_the_kernel_runs() {
    let ws = TestDir::new("tole-pqs-argv");
    plant_tole(&ws);
    // `./rmx -rf /X` runs `rm ./rmx -rf /X`.
    let Some(rm) = which::find_exe("rm") else {
        return;
    };
    write_executable(ws.join("rmx"), format!("#!{}\n", rm.display()));
    write_executable(ws.join("rmarg"), format!("#!{} -r\n", rm.display()));
    for line in [
        "./rmx -rf /X",
        "./rmx --recursive /X",
        "./rmarg /X",
        "./rmarg ../x",
    ] {
        let err = refused(&ws, line);
        assert!(err.contains("destructive"), "{line}: {err}");
    }
    // A plain script keeps its arguments, and so does what is harmless.
    write_executable(ws.join("plain"), "#!/bin/sh\n");
    let mut harmless = vec![
        "./plain a b --version",
        "./plain -rf x",
        "./rmx",
        "./rmx ./local",
    ];
    if let Some(env) = which::find_exe("env") {
        write_executable(ws.join("viaenv"), format!("#!{} python3\n", env.display()));
        harmless.push("./viaenv a -x");
    }
    for line in harmless {
        assert!(check(&ws, line).is_ok(), "{line}");
    }
    // The tail reaches the shell as the kernel gives it: `sh ./plain -c tole x`
    // reads `tole x` as a command text to a guard that cannot tell, and refuses.
    let argv = ["./plain", "-c", "tole x"].map(str::to_string);
    assert!(check_command_argv(&argv, &ws).is_err());
}

#[test]
fn nothing_is_allowed_that_the_same_command_by_bare_name_is_refused() {
    let ws = TestDir::new("tole-pqs-ab");
    plant_tole(&ws);
    write_executable(ws.join("plain"), "#!/bin/sh\n");
    let Some(rm) = which::find_exe("rm") else {
        return;
    };
    write_executable(ws.join("rmx"), format!("#!{}\n", rm.display()));
    let tails: [&[&str]; 8] = [
        &["-c", "tole x"],
        &["-c", "rm -rf /etc"],
        &["-c", "dd if=/dev/zero of=/dev/sda"],
        &["-rf", "/X"],
        &["-r", "../x"],
        &["env", "tole"],
        &["a", "tole"],
        &["--version"],
    ];
    for (script, bare) in [("./plain", "sh"), ("./rmx", "rm")] {
        for tail in tails {
            let rest: Vec<String> = tail.iter().map(|s| s.to_string()).collect();
            let path: Vec<String> = [script.to_string()]
                .into_iter()
                .chain(rest.clone())
                .collect();
            // What the kernel runs, by bare name.
            let by_name: Vec<String> = [bare.to_string(), script.to_string()]
                .into_iter()
                .chain(rest)
                .collect();
            let bare_refused = check_destructive_argv(&by_name).is_err()
                || tole_core::session_guard::check_session_argv(&by_name, &ws).is_err();
            if bare_refused {
                assert!(
                    check_command_argv(&path, &ws).is_err(),
                    "{script} {tail:?} is refused as `{bare}` and must be refused here"
                );
            }
        }
    }
}

#[test]
fn a_link_loop_says_so() {
    let ws = TestDir::new("tole-pqs-loop");
    std::os::unix::fs::symlink("b", ws.join("a")).unwrap();
    std::os::unix::fs::symlink("a", ws.join("b")).unwrap();
    let err = refused(&ws, "./a");
    assert!(err.contains("too many levels of symbolic links"), "{err}");
    assert!(!err.contains("no executable file"), "{err}");
}

#[test]
fn a_refused_interpreter_shows_the_argv_that_was_judged() {
    let ws = TestDir::new("tole-pqs-shown");
    let Some(sh) = which::find_exe("sh") else {
        return;
    };
    write_executable(ws.join("x.sh"), format!("#!{}\n", sh.display()));
    // A script argument that looks like `-c` is read by a shell as a command
    // string (as `sh x.sh -clean` is by the blocklist): refused, and the text
    // says which argv was judged.
    let argv = ["./x.sh", "-c", "rm -rf /etc"].map(str::to_string);
    let err = check_command_argv(&argv, &ws).unwrap_err();
    assert!(err.contains("the `#!` line of ./x.sh runs `"), "{err}");
    assert!(err.contains("./x.sh -c rm -rf /etc"), "{err}");
    assert!(err.contains("`, which is refused: "), "{err}");
}
