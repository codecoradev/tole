//! #209 provider failover through the REAL `tole run` binary: the primary
//! is unreachable, the reserve configured with `TOLE_FALLBACK_*` answers,
//! and the switch is recorded in the durable session log.

use serde_json::json;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::process::Command;
use std::time::Duration;

/// A provider that answers every chat completion with `text`.
fn serve_final(text: &'static str) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    std::thread::spawn(move || {
        for conn in listener.incoming() {
            let Ok(mut s) = conn else { continue };
            let _ = s.set_read_timeout(Some(Duration::from_secs(5)));
            let mut buf = Vec::new();
            let mut chunk = [0u8; 16384];
            loop {
                let n = s.read(&mut chunk).unwrap_or(0);
                if n == 0 {
                    break;
                }
                buf.extend_from_slice(&chunk[..n]);
                if let Some(h) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                    let head = String::from_utf8_lossy(&buf[..h]).to_ascii_lowercase();
                    let len: usize = head
                        .lines()
                        .find_map(|l| l.strip_prefix("content-length:"))
                        .and_then(|v| v.trim().parse().ok())
                        .unwrap_or(0);
                    if buf.len() >= h + 4 + len {
                        break;
                    }
                }
            }
            let body = json!({"choices":[{"message":{"role":"assistant","content":text}}],
                              "usage":{"prompt_tokens":2,"completion_tokens":1}})
            .to_string();
            let _ = s.write_all(
                format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                )
                .as_bytes(),
            );
        }
    });
    format!("http://{addr}/v1")
}

fn closed_port_url() -> String {
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = l.local_addr().unwrap().port();
    drop(l);
    format!("http://127.0.0.1:{port}/v1")
}

fn tmpdir(tag: &str) -> std::path::PathBuf {
    let d = std::env::temp_dir().join(format!("tole-failover-e2e-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn tole(dir: &std::path::Path, primary: &str, fallback: &[(&str, &str)]) -> std::process::Output {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_tole"));
    cmd.args(["--sessions-dir"])
        .arg(dir.join("sessions"))
        .args(["run", "say hi"])
        .current_dir(dir)
        .env("TOLE_BASE_URL", primary)
        .env("TOLE_MODEL", "m-primary")
        .env("TOLE_API_KEY", "sk-primary")
        .env("TOLE_STREAM", "0")
        .env("TOLE_NO_UPDATE_CHECK", "1")
        .env_remove("TOLE_MEMORY")
        .env_remove("TOLE_TRUST")
        .env_remove("OPENAI_BASE_URL")
        .env_remove("OPENAI_API_KEY")
        .env_remove("OPENAI_MODEL");
    for k in [
        "TOLE_FALLBACK_BASE_URL",
        "TOLE_FALLBACK_API_KEY",
        "TOLE_FALLBACK_MODEL",
    ] {
        cmd.env_remove(k);
    }
    for (k, v) in fallback {
        cmd.env(k, v);
    }
    cmd.output().unwrap()
}

#[test]
fn a_dead_primary_fails_over_to_the_reserve_and_the_switch_is_durable() {
    let dir = tmpdir("ok");
    let reserve = serve_final("FROM_RESERVE");
    let out = tole(
        &dir,
        &closed_port_url(),
        &[
            ("TOLE_FALLBACK_BASE_URL", &reserve),
            ("TOLE_FALLBACK_API_KEY", "sk-reserve"),
        ],
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "stdout={stdout}\nstderr={stderr}");
    assert!(stdout.contains("FROM_RESERVE"), "stdout={stdout}");
    // The session log records which provider answered and why.
    let log = std::fs::read_dir(dir.join("sessions"))
        .unwrap()
        .flatten()
        .find(|e| e.path().extension().is_some_and(|x| x == "jsonl"))
        .map(|e| std::fs::read_to_string(e.path()).unwrap())
        .expect("a session log");
    assert!(log.contains("\"tole_route\""), "{log}");
    assert!(
        log.contains("fallback: m-primary @ http://127.0.0.1"),
        "{log}"
    );
    assert!(log.contains("\"from\":\"primary: m-primary"), "{log}");
    // Neither key ends up in the durable log.
    assert!(!log.contains("sk-reserve") && !log.contains("sk-primary"));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_reserve_without_its_own_key_is_refused_loudly() {
    let dir = tmpdir("nokey");
    let out = tole(
        &dir,
        &closed_port_url(),
        &[("TOLE_FALLBACK_BASE_URL", "http://127.0.0.1:9/v1")],
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success());
    assert!(stderr.contains("TOLE_FALLBACK_API_KEY"), "{stderr}");
    assert!(!stderr.contains("sk-primary"), "{stderr}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn without_a_reserve_nothing_changes() {
    let dir = tmpdir("none");
    let primary = serve_final("FROM_PRIMARY");
    let out = tole(&dir, &primary, &[]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success() && stdout.contains("FROM_PRIMARY"),
        "{stdout}"
    );
    let log = std::fs::read_dir(dir.join("sessions"))
        .unwrap()
        .flatten()
        .find(|e| e.path().extension().is_some_and(|x| x == "jsonl"))
        .map(|e| std::fs::read_to_string(e.path()).unwrap())
        .unwrap();
    assert!(
        !log.contains("tole_route"),
        "a single provider records no route"
    );
    let _ = std::fs::remove_dir_all(&dir);
}
