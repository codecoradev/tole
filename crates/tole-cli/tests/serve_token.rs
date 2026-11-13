//! #682: the `tole serve` bearer token can come from a file, so it need not
//! sit in the process arguments, where every local process can read it
//! (`ps`, /proc/<pid>/cmdline). The argument form still works, with a
//! warning.

#![cfg(feature = "shell-tools")]

use std::io::{Read, Write};
use std::net::TcpStream;
use std::process::{Command, Stdio};
use std::time::Duration;

mod common;

const TOKEN: &str = "serve-file-token-0682-abcdef";

fn dir(tag: &str) -> std::path::PathBuf {
    let d = std::env::temp_dir().join(format!("tole-serve-token-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(d.join("home")).unwrap();
    d
}

fn status(port: u16, token: &str) -> String {
    let mut s = TcpStream::connect(("127.0.0.1", port)).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    let req = format!(
        "GET /sessions HTTP/1.1\r\nHost: x\r\nAuthorization: Bearer {token}\r\n\
         Connection: close\r\n\r\n"
    );
    s.write_all(req.as_bytes()).unwrap();
    let mut out = String::new();
    let _ = s.read_to_string(&mut out);
    out.lines().next().unwrap_or("").to_string()
}

#[test]
fn the_token_is_read_from_a_file_and_never_from_the_arguments() {
    let d = dir("file");
    let file = d.join("token");
    std::fs::write(&file, format!("{TOKEN}\n")).unwrap();
    let (mut child, port) = common::spawn_listening(|port| {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_tole"));
        cmd.args(["serve", "--port", &port.to_string(), "--token-file"])
            .arg(&file)
            .args(["-s"])
            .arg(d.join("sessions"))
            .env("CODECORA_HOME", d.join("home"))
            .env_remove("TOLE_SERVE_TOKEN")
            .stdin(Stdio::null())
            .stdout(Stdio::null());
        cmd
    });
    let cmdline = std::fs::read(format!("/proc/{}/cmdline", child.id())).unwrap_or_default();
    let ok = status(port, TOKEN);
    let bad = status(port, "not-the-token-at-all");
    let _ = child.kill();
    let _ = child.wait();
    assert!(ok.contains(" 200"), "{ok}");
    assert!(bad.contains(" 401"), "{bad}");
    assert!(
        !String::from_utf8_lossy(&cmdline).contains(TOKEN),
        "the token is not in the arguments"
    );
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn the_argument_form_warns_and_a_bad_file_stops_the_server() {
    let d = dir("warn");
    let run = |args: &[&str]| {
        Command::new(env!("CARGO_BIN_EXE_tole"))
            .args(args)
            .env("CODECORA_HOME", d.join("home"))
            .env_remove("TOLE_SERVE_TOKEN")
            .stdin(Stdio::null())
            .output()
            .unwrap()
    };
    // A blank flag value: the warning comes first, then the refusal.
    let out = run(&["serve", "--port", "1", "--token", " "]);
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success());
    assert!(err.contains("visible to every local process"), "{err}");
    assert!(err.contains("--token-file"), "{err}");

    let empty = d.join("empty");
    std::fs::write(&empty, "\n").unwrap();
    let out = run(&[
        "serve",
        "--port",
        "1",
        "--token-file",
        empty.to_str().unwrap(),
    ]);
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success() && err.contains("is empty"), "{err}");

    let out = run(&[
        "serve",
        "--token",
        "x",
        "--token-file",
        empty.to_str().unwrap(),
    ]);
    assert!(!out.status.success(), "the two forms conflict");
    let _ = std::fs::remove_dir_all(&d);
}
