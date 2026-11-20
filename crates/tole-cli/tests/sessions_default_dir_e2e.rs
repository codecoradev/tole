//! #866 follow-up: the DEFAULT sessions directory is `.tole/sessions` under the
//! process's working directory. When `--workspace` names a parent of that
//! directory (cwd `/repo/sub`, `--workspace /repo`) the sessions land in
//! `/repo/sub/.tole/sessions`: inside the sandbox's workspace and NOT under
//! `/repo/.tole`, which is what the `command` profile protects. `run_command`
//! is not affected (its sandbox workspace is the cwd, so `.tole` is the one
//! next to the sessions); `job_start` is (its workspace is `--workspace`).
//! Reproduced here with the real binary and a mock provider that asks for one
//! `job_start`: without the fix the job wrote into the sessions directory; now
//! the launch is refused (a layout the mount cannot protect) and nothing runs.
//!
//! Needs a working `bwrap` (`SKIPPED:` line otherwise; CI's `Sandbox
//! (bubblewrap)` job sets `TOLE_REQUIRE_BWRAP=1`).
#![cfg(all(unix, feature = "shell-tools"))]

mod common;
use common::TestDir;
use serde_json::{json, Value};
use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

type Seen = Arc<Mutex<Vec<Value>>>;

fn text(b: &[u8]) -> String {
    String::from_utf8_lossy(b).to_string()
}

fn completion(message: Value) -> Value {
    json!({"id": "c", "object": "chat.completion", "created": 1, "model": "mock",
        "choices": [{"index": 0, "message": message,
            "finish_reason": if message["tool_calls"].is_array() { "tool_calls" } else { "stop" }}],
        "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2}})
}

/// The first request gets a `run_command` that tries to write into the
/// sessions directory; the turn after the tool result gets a final answer.
fn reply(body: &Value) -> Value {
    let msgs = body["messages"].as_array().cloned().unwrap_or_default();
    let after_tool = msgs.last().is_some_and(|m| m["role"] == "tool");
    if after_tool {
        return completion(json!({"role": "assistant", "content": "done"}));
    }
    // `job_start` runs in the workspace root (`--workspace`), not the cwd.
    let cmd = "sh -c 'echo tamper >> sub/.tole/sessions/tamper.txt'";
    completion(
        json!({"role": "assistant", "content": null, "tool_calls": [{"id": "call_1",
        "type": "function", "function": {"name": "job_start",
        "arguments": json!({"command": cmd}).to_string()}}]}),
    )
}

fn spawn_mock() -> (String, Seen) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let seen: Seen = Arc::default();
    let seen2 = Arc::clone(&seen);
    std::thread::spawn(move || {
        for mut s in listener.incoming().flatten() {
            let mut buf = Vec::new();
            let mut chunk = [0u8; 16384];
            let mut body = Value::Null;
            let deadline = Instant::now() + Duration::from_secs(10);
            while Instant::now() < deadline {
                let n = s.read(&mut chunk).unwrap_or(0);
                buf.extend_from_slice(&chunk[..n]);
                if let Some(h) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                    if let Ok(v) = serde_json::from_slice::<Value>(&buf[h + 4..]) {
                        body = v;
                        break;
                    }
                }
                if n == 0 {
                    break;
                }
            }
            let data = reply(&body).to_string();
            seen2.lock().unwrap().push(body);
            let _ = s.write_all(
                format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{data}",
                    data.len()
                )
                .as_bytes(),
            );
        }
    });
    (format!("http://{addr}/v1"), seen)
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

/// `tole run --workspace <ws>` started in `<ws>/sub`, with `run_command`
/// required.
fn run_from_subdir(root: &Path, url: &str) -> (Output, PathBuf) {
    let ws = std::fs::canonicalize(root).unwrap().join("ws");
    let (sub, home) = (ws.join("sub"), root.join("home"));
    for d in [ws.join(".git/hooks"), sub.clone(), home.join("tole")] {
        std::fs::create_dir_all(d).unwrap();
    }
    std::fs::write(ws.join(".git/config"), "[core]\n").unwrap();
    std::fs::write(
        home.join("tole/config.toml"),
        "[sandbox.consumers]\njob_start = \"required\"\n",
    )
    .unwrap();
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_tole"));
    // Cleared first, then the hermetic HOME / CODECORA_HOME / PATH on top.
    cmd.env_clear();
    common::apply_hermetic_env(&mut cmd, &home, &[]);
    let out = cmd
        .args(["run", "--workspace"])
        .arg(&ws)
        .args(["--allow", "job_start", "touch the sessions"])
        .current_dir(&sub)
        .env("TOLE_NO_UPDATE_CHECK", "1")
        .env("TOLE_STREAM", "0")
        .env("TOLE_BASE_URL", url)
        .env("TOLE_MODEL", "mock-model")
        .env("TOLE_API_KEY", "k")
        .stdin(Stdio::null())
        .output()
        .unwrap();
    (out, ws)
}

#[test]
fn a_default_sessions_dir_below_the_workspace_root_is_not_writable_by_job_start() {
    if !bwrap_works() {
        return;
    }
    let (url, seen) = spawn_mock();
    let root = TestDir::new("tole-sess-default");
    let (out, ws) = run_from_subdir(&root, &url);
    let told: String = seen
        .lock()
        .unwrap()
        .iter()
        .flat_map(|b| b["messages"].as_array().cloned().unwrap_or_default())
        .filter(|m| m["role"] == "tool")
        .map(|m| m["content"].as_str().unwrap_or("").to_string())
        .collect::<Vec<_>>()
        .join("\n");
    let listing = Command::new("find")
        .arg(&ws)
        .output()
        .map(|o| text(&o.stdout))
        .unwrap_or_default();
    // A detached job runs on its own: give a vulnerable build time to show it.
    let tamper = ws.join("sub/.tole/sessions/tamper.txt");
    let until = Instant::now() + Duration::from_secs(2);
    while !tamper.exists() && Instant::now() < until {
        std::thread::sleep(Duration::from_millis(100));
    }
    let why = format!(
        "{}\n{}\n{told}\n{listing}",
        text(&out.stdout),
        text(&out.stderr)
    );
    assert!(
        !tamper.exists(),
        "the sandboxed job wrote into the sessions directory: {why}"
    );
    assert!(
        told.contains("the sessions directory") && told.contains("deeper than a direct child"),
        "the model is told why the command was refused: {why}"
    );
}

/// `tole sessions` started in a working directory that no longer exists (so a
/// relative sessions directory cannot be resolved), with `config` as the user
/// config. Returns stderr and whether it succeeded.
fn sessions_from_a_deleted_cwd(root: &Path, config: &str) -> (String, bool) {
    let (home, gone) = (root.join("home"), root.join("gone"));
    std::fs::create_dir_all(home.join("tole")).unwrap();
    std::fs::create_dir_all(&gone).unwrap();
    std::fs::write(home.join("tole/config.toml"), config).unwrap();
    // The shell enters the directory, removes it, and starts tole there.
    let script = format!(
        "cd {} && rmdir {} && exec {} sessions",
        gone.display(),
        gone.display(),
        env!("CARGO_BIN_EXE_tole")
    );
    let path = common::path_without_sibling_binaries_from("/usr/bin:/bin".as_ref());
    let mut cmd = Command::new("sh");
    cmd.env_clear();
    common::apply_hermetic_env(&mut cmd, &home, &[("PATH", path.to_str().unwrap())]);
    let out = cmd
        .args(["-c", &script])
        .env("TOLE_NO_UPDATE_CHECK", "1")
        .stdin(Stdio::null())
        .output()
        .unwrap();
    (text(&out.stderr), out.status.success())
}

/// A sessions directory that cannot be told to the sandbox (the working
/// directory is gone) must not fail open: where a command sandbox is required,
/// or always unattended (verify, stop gates), the run does not start.
#[test]
fn an_unresolvable_sessions_dir_refuses_the_start_when_a_sandbox_is_required() {
    let root = TestDir::new("tole-sess-nocwd-req");
    for cfg in [
        "[sandbox.consumers]\nrun_command = \"required\"\n",
        "[sandbox.consumers]\njob_start = \"required\"\n",
        "[sandbox.consumers]\nverify = \"optional\"\n",
        "[sandbox.consumers]\nturnend = \"optional\"\n",
    ] {
        let (err, ok) = sessions_from_a_deleted_cwd(&root, cfg);
        assert!(!ok, "{cfg}: must not start: {err}");
        assert!(
            err.contains(
                "refusing to start with a command sandbox that cannot protect the sessions directory"
            ),
            "{cfg}: {err}"
        );
    }
}

/// With only an `optional` run_command or job_start (a human attached) it is a
/// loud warning, not a refusal; with every consumer off nothing is said.
#[test]
fn an_unresolvable_sessions_dir_warns_for_optional_and_is_silent_when_off() {
    let root = TestDir::new("tole-sess-nocwd-opt");
    let (err, _) =
        sessions_from_a_deleted_cwd(&root, "[sandbox.consumers]\nrun_command = \"optional\"\n");
    assert!(
        err.contains("sandboxed commands are not kept from writing the sessions directory")
            && !err.contains("refusing to start"),
        "{err}"
    );
    // Written off: nothing sandboxed, nothing to say.
    let (err, _) = sessions_from_a_deleted_cwd(
        &root,
        "[sandbox.consumers]\nverify = \"off\"\nturnend = \"off\"\n",
    );
    assert!(
        !err.contains("sessions directory"),
        "no consumer, nothing to say: {err}"
    );
    // The default (#748, nothing written): a warning like an optional
    // run_command, never a refusal to start.
    let (err, _) = sessions_from_a_deleted_cwd(&root, "");
    assert!(
        err.contains("sandboxed commands are not kept from writing the sessions directory")
            && !err.contains("refusing to start"),
        "{err}"
    );
}
