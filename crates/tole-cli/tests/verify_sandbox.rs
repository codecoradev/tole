//! #748 slice 2: the mission `--verify` command under `[sandbox.consumers]
//! verify`, end to end with the REAL binary and a mock provider that always
//! declares the mission complete (so the verify command is the only thing
//! deciding the outcome).
//!
//! - `off` (the default): unchanged, the command runs as before.
//! - `required` / `optional` with no `bwrap`: the verify result is a REFUSAL.
//!   The mission fails closed (`verify_refused`, non-zero exit), the command
//!   never ran, no turn is spent retrying, and the model's `MISSION_COMPLETE`
//!   is not taken as success. `optional` counts as `required`: a mission is
//!   unattended.
//! - `required` with `bwrap`: the command writes the workspace but not
//!   `.git/hooks`, `.git/config` or `.tole`. Skipped with a `SKIPPED:` line
//!   when `bwrap` does not work here (`TOLE_REQUIRE_BWRAP=1` makes that a
//!   failure, as CI's `Sandbox (bubblewrap)` job sets it).

#![cfg(unix)]

mod common;
use common::TestDir;
use serde_json::json;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::Path;
use std::process::{Command, Stdio};

/// A provider that answers every request with the completion marker.
fn spawn_mock() -> String {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            std::thread::spawn(move || {
                let mut s = stream;
                let (mut buf, mut chunk) = (Vec::new(), [0u8; 16384]);
                let mut streamed = false;
                let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
                while std::time::Instant::now() < deadline {
                    let n = s.read(&mut chunk).unwrap_or(0);
                    buf.extend_from_slice(&chunk[..n]);
                    if let Some(h) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                        if let Ok(v) = serde_json::from_slice::<serde_json::Value>(&buf[h + 4..]) {
                            streamed = v.get("stream").and_then(|b| b.as_bool()) == Some(true);
                            break;
                        }
                    }
                    if n == 0 {
                        break;
                    }
                }
                let text = "Done.\nMISSION_COMPLETE";
                let resp = if streamed {
                    let mut out = String::new();
                    for part in [text, ""] {
                        out.push_str(&format!(
                            "data: {}\n\n",
                            json!({"choices": [{"index": 0,
                                "delta": {"content": part}, "finish_reason": null}]})
                        ));
                    }
                    out.push_str(&format!(
                        "data: {}\n\ndata: [DONE]\n\n",
                        json!({"choices": [], "usage": {"prompt_tokens": 1,
                               "completion_tokens": 1, "total_tokens": 2}})
                    ));
                    format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                        out.len(),
                        out
                    )
                } else {
                    let data = json!({
                        "id": "c", "object": "chat.completion", "created": 1, "model": "mock",
                        "choices": [{"index": 0, "message": {"role": "assistant",
                            "content": text}, "finish_reason": "stop"}],
                        "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2}
                    })
                    .to_string();
                    format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                        data.len(),
                        data
                    )
                };
                let _ = s.write_all(resp.as_bytes());
            });
        }
    });
    format!("http://{addr}/v1")
}

struct Run {
    code: i32,
    stdout: String,
    stderr: String,
}

/// `tole mission --verify <verify>` in `ws` (the process's working directory,
/// where the verify command runs) with `user_config` as the user config.
/// `path` is the child's PATH.
fn mission(ws: &Path, home: &Path, user_config: &str, path: &str, verify: &str) -> Run {
    std::fs::create_dir_all(home.join("tole")).unwrap();
    std::fs::write(home.join("tole/config.toml"), user_config).unwrap();
    let sessions = home.join("sessions");
    let out = Command::new(env!("CARGO_BIN_EXE_tole"))
        .args(["mission", "-s"])
        .arg(&sessions)
        .arg("--workspace")
        .arg(ws)
        .args(["--yes", "--verify", verify, "do it"])
        .current_dir(ws)
        .env_clear()
        .env("PATH", path)
        .env("TOLE_NO_UPDATE_CHECK", "1")
        .env("CODECORA_HOME", home)
        .env("TOLE_BASE_URL", spawn_mock())
        .env("TOLE_MODEL", "mock-model")
        .env("TOLE_API_KEY", "sk-test")
        .stdin(Stdio::null())
        .output()
        .expect("run tole");
    Run {
        code: out.status.code().unwrap_or(-1),
        stdout: String::from_utf8_lossy(&out.stdout).to_string(),
        stderr: String::from_utf8_lossy(&out.stderr).to_string(),
    }
}

fn path_env() -> String {
    std::env::var("PATH").unwrap_or_default()
}

/// A PATH with `sh` and `touch` but no `bwrap`: no sandbox backend.
fn path_without_bwrap(dir: &Path) -> String {
    let bin = dir.join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    for p in ["sh", "touch"] {
        let src = ["/bin", "/usr/bin"]
            .iter()
            .map(|d| Path::new(d).join(p))
            .find(|c| c.exists())
            .unwrap();
        std::os::unix::fs::symlink(src, bin.join(p)).unwrap();
    }
    bin.display().to_string()
}

fn bwrap_works() -> bool {
    let ok = Command::new("bwrap")
        .args([
            "--unshare-all",
            "--unshare-user",
            "--disable-userns",
            "--ro-bind",
            "/",
            "/",
            "true",
        ])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    if !ok {
        assert!(
            std::env::var("TOLE_REQUIRE_BWRAP").is_err(),
            "TOLE_REQUIRE_BWRAP is set but no sandbox works here"
        );
        eprintln!("SKIPPED: no working bwrap here");
    }
    ok
}

fn project(tag: &str) -> (TestDir, std::path::PathBuf, std::path::PathBuf) {
    let dir = TestDir::new(&format!("tole-vsbx-{tag}"));
    let (ws, home) = (dir.join("ws"), dir.join("home"));
    for d in [ws.join(".git/hooks"), ws.join(".tole"), home.clone()] {
        std::fs::create_dir_all(d).unwrap();
    }
    std::fs::write(ws.join(".git/config"), "[core]\n").unwrap();
    (dir, ws, home)
}

#[test]
fn verify_off_is_unchanged_and_explicit_off_is_the_same() {
    for (tag, cfg) in [
        ("default", ""),
        ("off", "[sandbox.consumers]\nverify = \"off\"\n"),
    ] {
        let (dir, ws, home) = project(tag);
        let cmd = "touch verify-ran; echo x > .git/hooks/h";
        let r = mission(&ws, &home, cfg, &path_env(), cmd);
        assert_eq!(r.code, 0, "{}\n{}", r.stdout, r.stderr);
        assert!(r.stdout.contains("status: complete"), "{}", r.stdout);
        assert!(ws.join("verify-ran").exists(), "{tag}: the command ran");
        assert!(
            ws.join(".git/hooks/h").exists(),
            "{tag}: off is today's behaviour, nothing is protected"
        );
        drop(dir);
    }
}

/// The key fail-closed test: no sandbox, so no verify, so no completion.
#[test]
fn a_required_verify_without_a_sandbox_is_refused_and_the_mission_fails_closed() {
    let (dir, ws, home) = project("required");
    let path = path_without_bwrap(&dir);
    let r = mission(
        &ws,
        &home,
        "[sandbox.consumers]\nverify = \"required\"\n",
        &path,
        "touch verify-ran",
    );
    assert_ne!(r.code, 0, "a refused verify is not a success: {}", r.stdout);
    assert!(r.stdout.contains("status: verify_refused"), "{}", r.stdout);
    assert!(
        !r.stdout.contains("status: complete"),
        "the model's MISSION_COMPLETE must not stand: {}",
        r.stdout
    );
    assert!(
        r.stderr.contains("verify refused")
            && r.stderr.contains("sandbox required but unavailable")
            && r.stderr.contains("sandbox.consumers.verify"),
        "the reason names the key: {}",
        r.stderr
    );
    assert!(!ws.join("verify-ran").exists(), "the command never ran");
    // Not retried against the model: one turn, no verify failures counted.
    assert!(
        r.stdout.contains("turns: 1") && r.stdout.contains("verify failures: 0"),
        "{}",
        r.stdout
    );
    drop(dir);
}

/// A mission is unattended: `optional` is `required`.
#[test]
fn an_optional_verify_without_a_sandbox_is_refused_too() {
    let (dir, ws, home) = project("optional");
    let path = path_without_bwrap(&dir);
    let r = mission(
        &ws,
        &home,
        "[sandbox.consumers]\nverify = \"optional\"\n",
        &path,
        "touch verify-ran",
    );
    assert_ne!(r.code, 0, "{}", r.stdout);
    assert!(r.stdout.contains("status: verify_refused"), "{}", r.stdout);
    assert!(r.stderr.contains("unattended"), "{}", r.stderr);
    assert!(!ws.join("verify-ran").exists(), "never ran unsandboxed");
    drop(dir);
}

/// With a working bubblewrap: the workspace is writable, the paths that run
/// code later outside the sandbox are not, and the exit status still decides.
#[test]
fn a_sandboxed_verify_writes_the_workspace_but_not_hooks_config_or_tole() {
    if !bwrap_works() {
        return;
    }
    let (dir, ws, home) = project("bwrap");
    // Passes only when every protected write was refused and the workspace
    // write worked.
    let cmd = "echo ok > made && \
               ! (echo x > .git/hooks/pre-commit) 2>/dev/null && \
               ! (echo x >> .git/config) 2>/dev/null && \
               ! (echo x > .tole/evil) 2>/dev/null";
    let r = mission(
        &ws,
        &home,
        "[sandbox.consumers]\nverify = \"required\"\n",
        &path_env(),
        cmd,
    );
    assert_eq!(r.code, 0, "{}\n{}", r.stdout, r.stderr);
    assert!(r.stdout.contains("status: complete"), "{}", r.stdout);
    assert!(
        ws.join("made").exists(),
        "the workspace write reached the host"
    );
    for p in [".git/hooks/pre-commit", ".tole/evil"] {
        assert!(!ws.join(p).exists(), "{p} must not be created");
    }
    assert_eq!(
        std::fs::read_to_string(ws.join(".git/config")).unwrap(),
        "[core]\n"
    );
    drop(dir);
}

/// A failing sandboxed verify behaves as a failing plain one: the model is
/// told, three strikes settle `verify_failed`.
#[test]
fn a_failing_sandboxed_verify_is_a_verify_failure_not_a_refusal() {
    if !bwrap_works() {
        return;
    }
    let (dir, ws, home) = project("bwrap-fail");
    let r = mission(
        &ws,
        &home,
        "[sandbox.consumers]\nverify = \"required\"\n",
        &path_env(),
        "echo nope; exit 3",
    );
    assert_ne!(r.code, 0);
    assert!(r.stdout.contains("status: verify_failed"), "{}", r.stdout);
    assert!(r.stdout.contains("verify failures: 3"), "{}", r.stdout);
    drop(dir);
}
