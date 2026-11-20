//! #748 slice 2: the mission `--verify` command under `[sandbox.consumers]
//! verify`, end to end with the REAL binary and a mock provider that always
//! declares the mission complete (so the verify command is the only thing
//! deciding the outcome).
//!
//! - `off` (explicit): unchanged, the command runs as before. The default
//!   (no key) is `optional` since #748's flip: see the end of this file.
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

/// The `tole` binary on an otherwise empty environment, plus the shared
/// hermetic HOME / CODECORA_HOME. The caller sets `PATH` and the rest.
fn scrubbed_tole(home: &Path) -> Command {
    let mut c = Command::new(env!("CARGO_BIN_EXE_tole"));
    c.env_clear();
    common::apply_hermetic_env(&mut c, home, &[]);
    c
}

/// `tole mission --verify <verify>` in `ws` (the process's working directory,
/// where the verify command runs) with `user_config` as the user config.
/// `path` is the child's PATH.
fn mission(ws: &Path, home: &Path, user_config: &str, path: &str, verify: &str) -> Run {
    mission_in(ws, home, user_config, path, verify, &home.join("sessions"))
}

/// [`mission`] with the sessions directory given (`-s`).
fn mission_in(
    ws: &Path,
    home: &Path,
    user_config: &str,
    path: &str,
    verify: &str,
    sessions: &Path,
) -> Run {
    std::fs::create_dir_all(home.join("tole")).unwrap();
    std::fs::write(home.join("tole/config.toml"), user_config).unwrap();
    let out = scrubbed_tole(home)
        .args(["mission", "-s"])
        .arg(sessions)
        .arg("--workspace")
        .arg(ws)
        .args(["--yes", "--verify", verify, "do it"])
        .current_dir(ws)
        .env("PATH", path)
        .env("TOLE_NO_UPDATE_CHECK", "1")
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
    for p in ["sh", "touch", "chmod"] {
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
fn verify_off_is_unchanged_when_explicit() {
    let (dir, ws, home) = project("off");
    let cmd = "touch verify-ran; echo x > .git/hooks/h";
    let cfg = "[sandbox.consumers]\nverify = \"off\"\n";
    let r = mission(&ws, &home, cfg, &path_env(), cmd);
    assert_eq!(r.code, 0, "{}\n{}", r.stdout, r.stderr);
    assert!(r.stdout.contains("status: complete"), "{}", r.stdout);
    assert!(ws.join("verify-ran").exists(), "the command ran");
    assert!(
        ws.join(".git/hooks/h").exists(),
        "off is today's behaviour, nothing is protected"
    );
    assert!(!r.stderr.contains("UNSANDBOXED"), "{}", r.stderr);
    drop(dir);
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
    // Stopped by the start-up check: no model turn was spent, nothing counted.
    assert!(
        r.stdout.contains("turns: 0") && r.stdout.contains("verify failures: 0"),
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
    assert!(r.stdout.contains("turns: 0"), "{}", r.stdout);
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

/// The exit codes `bwrap` and a shell use for their own failures (125, 126,
/// 127) are the COMMAND's when the command ends with them: `bwrap` reported
/// the command's exit code, so it is an ordinary failure the model is told
/// about (three strikes, `verify_failed`), not a refusal.
#[test]
fn a_verify_exiting_125_126_or_127_is_a_failure_not_a_refusal() {
    if !bwrap_works() {
        return;
    }
    for code in [125, 126, 127] {
        let (dir, ws, home) = project(&format!("exit-{code}"));
        let r = mission(
            &ws,
            &home,
            "[sandbox.consumers]\nverify = \"required\"\n",
            &path_env(),
            &format!("echo out-{code}; exit {code}"),
        );
        let why = format!("exit {code}\n{}\n{}", r.stdout, r.stderr);
        assert_ne!(r.code, 0, "{why}");
        assert!(r.stdout.contains("status: verify_failed"), "{why}");
        assert!(!r.stdout.contains("status: verify_refused"), "{why}");
        assert!(r.stdout.contains("verify failures: 3"), "{why}");
        assert!(!r.stderr.contains("verify refused"), "{why}");
        drop(dir);
    }
}

/// A command cannot change how it is classified by writing to the launcher's
/// status descriptor (fd 3). With bubblewrap 0.9.0 the descriptor is closed
/// inside the sandbox; with a version that leaves it open the command could
/// write a line, but `bwrap` writes its own `exit-code` last and it wins. Either
/// way a failing command stays a failure and a passing one passes.
#[test]
fn a_verify_writing_to_the_status_descriptor_cannot_change_its_classification() {
    if !bwrap_works() {
        return;
    }
    let forged =
        r#"echo '{"exit-code": 0}' >&3 2>/dev/null; echo '{"exit-code": 3}' >&3 2>/dev/null;"#;
    for (tag, cmd, status, failures) in [
        ("fail", format!("{forged} exit 3"), "verify_failed", "3"),
        ("pass", format!("{forged} exit 0"), "complete", "0"),
    ] {
        let (dir, ws, home) = project(&format!("fd3-{tag}"));
        let r = mission(
            &ws,
            &home,
            "[sandbox.consumers]\nverify = \"required\"\n",
            &path_env(),
            &cmd,
        );
        let why = format!("{tag}\n{}\n{}", r.stdout, r.stderr);
        assert!(r.stdout.contains(&format!("status: {status}")), "{why}");
        assert!(
            r.stdout.contains(&format!("verify failures: {failures}")),
            "{why}"
        );
        assert!(!r.stdout.contains("verify_refused"), "{why}");
        drop(dir);
    }
}

/// A PATH whose `bwrap` is a script: `--version` and the self-test (`... true`)
/// succeed, so the sandbox looks usable; then `body` runs for every real
/// launch. With `chmod_after_selftest` the script removes its own execute bit
/// once it has passed the self-test, so a later launch cannot be spawned.
fn path_with_fake_bwrap(dir: &Path, body: &str, chmod_after_selftest: bool) -> String {
    use std::os::unix::fs::PermissionsExt;
    let bin = dir.join("fakebin");
    std::fs::create_dir_all(&bin).unwrap();
    let smoke = if chmod_after_selftest {
        "chmod a-x \"$0\""
    } else {
        ":"
    };
    let script = format!(
        "#!/bin/sh\n\
         case \"$1\" in --version) echo 'bubblewrap 0.9.0'; exit 0;; esac\n\
         for a in \"$@\"; do last=$a; done\n\
         if [ \"$last\" = true ]; then {smoke}; exit 0; fi\n\
         {body}\n"
    );
    let fake = bin.join("bwrap");
    std::fs::write(&fake, script).unwrap();
    std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
    format!("{}:{}", bin.display(), path_without_bwrap(dir))
}

/// A launcher that fails (bwrap's own error and exit 1, no exit code of a
/// command) is the sandbox's failure, not the command's: the mission ends
/// `verify_refused`, the model is not told to fix it, no strike is counted,
/// and it is never a pass. A real child exit 3 stays an ordinary failure
/// (`a_failing_sandboxed_verify_is_a_verify_failure_not_a_refusal`).
#[test]
fn a_launcher_failure_is_a_refusal_not_a_verify_failure() {
    let (dir, ws, home) = project("launcher");
    let path = path_with_fake_bwrap(
        &dir,
        "echo 'bwrap: setting up uid map: Permission denied' >&2; exit 1",
        false,
    );
    let r = mission(
        &ws,
        &home,
        "[sandbox.consumers]\nverify = \"required\"\n",
        &path,
        "touch verify-ran",
    );
    assert_ne!(r.code, 0, "{}\n{}", r.stdout, r.stderr);
    assert!(r.stdout.contains("status: verify_refused"), "{}", r.stdout);
    assert!(!r.stdout.contains("status: verify_failed"), "{}", r.stdout);
    assert!(
        r.stderr
            .contains("the sandbox failed before or while starting")
            && r.stderr.contains("setting up uid map"),
        "{}",
        r.stderr
    );
    // The launcher passed the start-up check, so the model turn happened; the
    // failure was not handed back as a verify failure.
    assert!(r.stdout.contains("verify failures: 0"), "{}", r.stdout);
    assert!(!ws.join("verify-ran").exists());
    drop(dir);
}

/// A workspace layout the command profile refuses (an uninitialized
/// submodule, a linked worktree inside the workspace) is a refused verify, not
/// a silent skip and not an unsandboxed run: the mission ends `verify_refused`
/// before a model turn, says why, and the command never runs. A mission is
/// unattended, so `optional` does not fall back either (`run_command` would,
/// with a human attached).
#[test]
fn a_workspace_layout_the_profile_refuses_refuses_the_verify() {
    if !bwrap_works() {
        return;
    }
    for mode in ["required", "optional", "default"] {
        for layout in ["submodule", "worktree"] {
            let (dir, ws, home) = project(&format!("layout-{layout}-{mode}"));
            let real = std::fs::canonicalize(&ws).unwrap();
            let needle = match layout {
                "submodule" => {
                    std::fs::write(
                        ws.join(".gitmodules"),
                        "[submodule \"s\"]\n\tpath = s\n\turl = ../s\n",
                    )
                    .unwrap();
                    "not initialized"
                }
                _ => {
                    std::fs::create_dir_all(ws.join(".git/worktrees/w1")).unwrap();
                    std::fs::create_dir_all(ws.join("wt")).unwrap();
                    std::fs::write(ws.join("wt/.git"), "gitdir: x\n").unwrap();
                    std::fs::write(
                        ws.join(".git/worktrees/w1/gitdir"),
                        format!("{}/wt/.git\n", real.display()),
                    )
                    .unwrap();
                    "linked worktree"
                }
            };
            let r = mission(
                &ws,
                &home,
                &match mode {
                    "default" => String::new(),
                    _ => format!("[sandbox.consumers]\nverify = \"{mode}\"\n"),
                },
                &path_env(),
                "touch verify-ran",
            );
            let why = format!("{layout}/{mode}\n{}\n{}", r.stdout, r.stderr);
            assert_ne!(r.code, 0, "{why}");
            assert!(r.stdout.contains("status: verify_refused"), "{why}");
            assert!(!r.stdout.contains("status: complete"), "{why}");
            assert!(
                r.stderr.contains("verify refused") && r.stderr.contains(needle),
                "the reason is shown: {why}"
            );
            assert!(
                r.stdout.contains("turns: 0"),
                "stopped before a turn: {why}"
            );
            assert!(!ws.join("verify-ran").exists(), "never ran: {why}");
            drop(dir);
        }
    }
}

/// The launcher cannot even be started (it passed the self-test and was
/// gone after): the spawn error is a refusal too.
#[test]
fn a_launcher_that_cannot_be_spawned_is_a_refusal() {
    let (dir, ws, home) = project("spawn");
    let path = path_with_fake_bwrap(&dir, "exit 1", true);
    let r = mission(
        &ws,
        &home,
        "[sandbox.consumers]\nverify = \"required\"\n",
        &path,
        "touch verify-ran",
    );
    assert_ne!(r.code, 0, "{}\n{}", r.stdout, r.stderr);
    assert!(r.stdout.contains("status: verify_refused"), "{}", r.stdout);
    assert!(
        r.stderr.contains("the sandbox did not start"),
        "{}",
        r.stderr
    );
    assert!(r.stdout.contains("verify failures: 0"), "{}", r.stdout);
    drop(dir);
}

/// `tole run --on-turnend <gate> "goal"` in `ws` with `user_config`.
#[cfg(feature = "shell-tools")]
fn run_with_gate(ws: &Path, home: &Path, user_config: &str, path: &str, gate: &Path) -> Run {
    std::fs::create_dir_all(home.join("tole")).unwrap();
    std::fs::write(home.join("tole/config.toml"), user_config).unwrap();
    let out = scrubbed_tole(home)
        .args(["run", "-s"])
        .arg(home.join("sessions"))
        .arg("--workspace")
        .arg(ws)
        .arg("--on-turnend")
        .arg(gate)
        .arg("do it")
        .current_dir(ws)
        .env("PATH", path)
        .env("TOLE_NO_UPDATE_CHECK", "1")
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

#[cfg(feature = "shell-tools")]
fn gate_script(ws: &Path, body: &str) -> std::path::PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let gate = ws.join("gate.sh");
    std::fs::write(&gate, format!("#!/bin/sh\n{body}\n")).unwrap();
    std::fs::set_permissions(&gate, std::fs::Permissions::from_mode(0o755)).unwrap();
    gate
}

/// The stop gate with `off` is today's: it runs (here it passes, so the
/// run finishes), and nothing is protected.
#[test]
#[cfg(feature = "shell-tools")]
fn a_stop_gate_with_the_sandbox_off_is_unchanged() {
    let (dir, ws, home) = project("gate-off");
    let gate = gate_script(&ws, "touch gate-ran; echo x > .git/hooks/h; exit 0");
    let r = run_with_gate(
        &ws,
        &home,
        "[sandbox.consumers]\nturnend = \"off\"\n",
        &path_env(),
        &gate,
    );
    assert_eq!(r.code, 0, "{}\n{}", r.stdout, r.stderr);
    assert!(ws.join("gate-ran").exists() && ws.join(".git/hooks/h").exists());
    drop(dir);
}

/// A required sandbox that is missing is a DENY of the stop gate: the gate
/// never ran, the model may not finish on its own say-so, and the run ends
/// blocked (exit 6) with the reason.
#[test]
#[cfg(feature = "shell-tools")]
fn a_required_stop_gate_without_a_sandbox_blocks_the_turn() {
    let (dir, ws, home) = project("gate-required");
    let gate = gate_script(&ws, "touch gate-ran; exit 0");
    let r = run_with_gate(
        &ws,
        &home,
        "[sandbox.consumers]\nturnend = \"required\"\n",
        &path_without_bwrap(&dir),
        &gate,
    );
    assert_eq!(
        r.code, 6,
        "blocked, not finished: {}\n{}",
        r.stdout, r.stderr
    );
    assert!(
        r.stderr.contains("stop gate blocked the turn")
            && r.stderr.contains("sandbox required but unavailable")
            && r.stderr.contains("sandbox.consumers.turnend"),
        "{}",
        r.stderr
    );
    assert!(!ws.join("gate-ran").exists(), "the gate never ran");
    drop(dir);
}

/// With bubblewrap the gate runs sandboxed and its exit status still passes
/// the turn.
#[test]
#[cfg(feature = "shell-tools")]
fn a_sandboxed_stop_gate_cannot_write_hooks_and_still_passes_the_turn() {
    if !bwrap_works() {
        return;
    }
    let (dir, ws, home) = project("gate-bwrap");
    let gate = gate_script(
        &ws,
        "echo ok > gate-made; ! (echo x > .git/hooks/h) 2>/dev/null || { echo hooks writable; exit 1; }",
    );
    let r = run_with_gate(
        &ws,
        &home,
        "[sandbox.consumers]\nturnend = \"required\"\n",
        &path_env(),
        &gate,
    );
    assert_eq!(r.code, 0, "{}\n{}", r.stdout, r.stderr);
    assert!(ws.join("gate-made").exists());
    assert!(!ws.join(".git/hooks/h").exists());
    drop(dir);
}

/// A launcher that fails is the sandbox's failure, not the gate's verdict: the
/// turn is still denied (never a pass, exit 6 after the 3-denial cap), but the
/// reason says the sandbox failed before or while starting the gate, and the
/// gate never ran.
#[test]
#[cfg(feature = "shell-tools")]
fn a_stop_gate_whose_launcher_fails_is_a_deny_that_says_so() {
    let (dir, ws, home) = project("gate-launcher");
    let gate = gate_script(&ws, "touch gate-ran; exit 0");
    let path = path_with_fake_bwrap(
        &dir,
        "echo 'bwrap: setting up uid map: Permission denied' >&2; exit 1",
        false,
    );
    let r = run_with_gate(
        &ws,
        &home,
        "[sandbox.consumers]\nturnend = \"required\"\n",
        &path,
        &gate,
    );
    assert_eq!(
        r.code, 6,
        "denied, never finished: {}\n{}",
        r.stdout, r.stderr
    );
    assert!(
        r.stderr.contains("stop gate blocked the turn")
            && r.stderr
                .contains("the sandbox failed before or while starting the gate")
            && r.stderr.contains("setting up uid map"),
        "{}",
        r.stderr
    );
    assert!(!ws.join("gate-ran").exists(), "the gate never ran");
    drop(dir);
}

/// A launcher that cannot be spawned after the self-test is a deny too.
#[test]
#[cfg(feature = "shell-tools")]
fn a_stop_gate_whose_launcher_cannot_be_spawned_is_a_deny() {
    let (dir, ws, home) = project("gate-spawn");
    let gate = gate_script(&ws, "touch gate-ran; exit 0");
    let path = path_with_fake_bwrap(&dir, "exit 1", true);
    let r = run_with_gate(
        &ws,
        &home,
        "[sandbox.consumers]\nturnend = \"required\"\n",
        &path,
        &gate,
    );
    assert_eq!(r.code, 6, "{}\n{}", r.stdout, r.stderr);
    assert!(
        r.stderr.contains("the sandbox did not start"),
        "{}",
        r.stderr
    );
    assert!(!ws.join("gate-ran").exists());
    drop(dir);
}

/// A gate that really exits 125, 126 or 127 is an ordinary deny with its own
/// output (the exit codes are the gate's, `bwrap` reported them), and one that
/// exits 0 passes even when it writes to the status descriptor.
#[test]
#[cfg(feature = "shell-tools")]
fn a_sandboxed_gate_exiting_125_126_127_is_its_own_verdict() {
    if !bwrap_works() {
        return;
    }
    for code in [125, 126, 127] {
        let (dir, ws, home) = project(&format!("gate-exit-{code}"));
        let gate = gate_script(&ws, &format!("echo gate-said-{code}; exit {code}"));
        let r = run_with_gate(
            &ws,
            &home,
            "[sandbox.consumers]\nturnend = \"required\"\n",
            &path_env(),
            &gate,
        );
        assert_eq!(r.code, 6, "{code}: {}\n{}", r.stdout, r.stderr);
        assert!(
            r.stderr.contains(&format!("gate-said-{code}"))
                && !r.stderr.contains("the sandbox failed"),
            "{code}: the gate's own output, not a launcher failure: {}",
            r.stderr
        );
        drop(dir);
    }
    let (dir, ws, home) = project("gate-fd3");
    let gate = gate_script(
        &ws,
        r#"echo '{"exit-code": 3}' >&3 2>/dev/null; echo ok > gate-made; exit 0"#,
    );
    let r = run_with_gate(
        &ws,
        &home,
        "[sandbox.consumers]\nturnend = \"required\"\n",
        &path_env(),
        &gate,
    );
    assert_eq!(r.code, 0, "{}\n{}", r.stdout, r.stderr);
    assert!(ws.join("gate-made").exists());
    drop(dir);
}

/// #866: with the sessions directory moved inside the workspace (`-s
/// ./sessions`), a sandboxed verify command cannot write into it (it is bound
/// read-only like `.tole`), while the mission itself, a host process, still
/// writes its session there.
#[test]
fn a_sandboxed_verify_cannot_write_a_sessions_dir_inside_the_workspace() {
    if !bwrap_works() {
        return;
    }
    let (dir, ws, home) = project("sessions-in-ws");
    let sessions = ws.join("sessions");
    let cmd = "! (touch sessions/evil) 2>/dev/null && ! (rm -f sessions/*.jsonl) 2>/dev/null \
               && test -n \"$(ls sessions)\"";
    let r = mission_in(
        &ws,
        &home,
        "[sandbox.consumers]\nverify = \"required\"\n",
        &path_env(),
        cmd,
        &sessions,
    );
    assert_eq!(r.code, 0, "{}\n{}", r.stdout, r.stderr);
    assert!(r.stdout.contains("status: complete"), "{}", r.stdout);
    assert!(!sessions.join("evil").exists(), "the write was refused");
    let files: Vec<_> = std::fs::read_dir(&sessions)
        .unwrap()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    assert!(
        files.iter().any(|f| f.ends_with(".jsonl")),
        "the host wrote the session there: {files:?}"
    );
    drop(dir);
}

/// A sessions directory deeper than a direct child is refused (a mount cannot
/// protect it): the mission ends `verify_refused` before a turn, with the
/// reason, and the verify never runs.
#[test]
fn a_sessions_dir_that_cannot_be_protected_refuses_the_verify() {
    if !bwrap_works() {
        return;
    }
    let (dir, ws, home) = project("sessions-deep");
    let sessions = ws.join("a/b");
    std::fs::create_dir_all(&sessions).unwrap();
    let r = mission_in(
        &ws,
        &home,
        "[sandbox.consumers]\nverify = \"required\"\n",
        &path_env(),
        "touch verify-ran",
        &sessions,
    );
    assert_ne!(r.code, 0, "{}\n{}", r.stdout, r.stderr);
    assert!(r.stdout.contains("status: verify_refused"), "{}", r.stdout);
    assert!(
        r.stderr.contains("the sessions directory")
            && r.stderr.contains("deeper than a direct child"),
        "{}",
        r.stderr
    );
    assert!(!ws.join("verify-ran").exists(), "never ran");
    drop(dir);
}

// ---------------------------------------------------------------------------
// #748: the default flip. With no `verify` / `turnend` key, the consumer is
// `optional` BY DEFAULT: sandboxed where a backend works, a loud unsandboxed
// run only where the host has none at all, and a refusal for everything else.
// An explicit `optional` / `required` (above) never falls back.
// ---------------------------------------------------------------------------

/// The ledger line the fallback records (`<home>/tole/plugin-ledger.jsonl`).
fn fallback_lines(home: &Path, consumer: &str) -> Vec<serde_json::Value> {
    std::fs::read_to_string(home.join("tole/plugin-ledger.jsonl"))
        .unwrap_or_default()
        .lines()
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        .filter(|v| v["event"] == "sandbox_fallback" && v["consumer"] == consumer)
        .collect()
}

/// A user config without the keys (an empty file, or only unrelated ones) is
/// an existing config from before the flip: its verify is sandboxed now, with
/// no file migrated. The command passes only when the protected writes fail.
#[test]
fn a_config_without_the_keys_sandboxes_verify_by_default() {
    if !bwrap_works() {
        return;
    }
    for (tag, cfg) in [
        ("empty", ""),
        ("unrelated", "[sandbox.consumers]\nrun_command = \"off\"\n"),
    ] {
        let (dir, ws, home) = project(&format!("dflt-{tag}"));
        let cmd = "echo ok > made && ! (echo x > .git/hooks/pre-commit) 2>/dev/null \
                   && ! (echo x > .tole/evil) 2>/dev/null";
        let r = mission(&ws, &home, cfg, &path_env(), cmd);
        assert_eq!(r.code, 0, "{tag}\n{}\n{}", r.stdout, r.stderr);
        assert!(r.stdout.contains("status: complete"), "{tag}: {}", r.stdout);
        assert!(
            ws.join("made").exists(),
            "{tag}: the workspace write worked"
        );
        assert!(!ws.join(".git/hooks/pre-commit").exists(), "{tag}");
        assert!(
            fallback_lines(&home, "verify").is_empty(),
            "{tag}: no fallback"
        );
        drop(dir);
    }
}

/// A host with no backend at all: the default runs the command unsandboxed,
/// says so on stderr and puts a `sandbox_fallback` line in the ledger. The
/// mission completes (it is not refused, as an explicit `optional` is).
#[test]
fn the_default_verify_without_any_sandbox_runs_loudly_and_is_ledgered() {
    let (dir, ws, home) = project("dflt-nosandbox");
    let r = mission(
        &ws,
        &home,
        "",
        &path_without_bwrap(&dir),
        "touch verify-ran",
    );
    assert_eq!(r.code, 0, "{}\n{}", r.stdout, r.stderr);
    assert!(r.stdout.contains("status: complete"), "{}", r.stdout);
    assert!(ws.join("verify-ran").exists(), "ran, unsandboxed");
    assert!(
        r.stderr.contains("WARNING: verify runs UNSANDBOXED")
            && r.stderr.contains("optional by default")
            && r.stderr.contains("sandbox.consumers.verify = \"off\""),
        "{}",
        r.stderr
    );
    let lines = fallback_lines(&home, "verify");
    assert_eq!(lines.len(), 1, "one ledger line: {lines:?}");
    assert_eq!(lines[0]["kind"], "sandbox");
    assert_eq!(lines[0]["default"], true);
    assert!(lines[0]["reason"].as_str().unwrap().contains("bwrap"));
    // An explicit `off` is silent and records nothing.
    let (dir2, ws2, home2) = project("dflt-nosandbox-off");
    let r = mission(
        &ws2,
        &home2,
        "[sandbox.consumers]\nverify = \"off\"\n",
        &path_without_bwrap(&dir2),
        "touch verify-ran",
    );
    assert_eq!(r.code, 0, "{}", r.stderr);
    assert!(!r.stderr.contains("UNSANDBOXED"), "{}", r.stderr);
    assert!(fallback_lines(&home2, "verify").is_empty());
    drop((dir, dir2));
}

/// The default degrades only for "no backend at all". A backend that exists
/// and then fails (the self-test passes, the launch does not) is a refusal,
/// as with `required`: fail closed when we could sandbox and something is wrong.
#[test]
fn the_default_verify_whose_launcher_fails_is_still_refused() {
    for (tag, body, chmod) in [
        (
            "fails",
            "echo 'bwrap: setting up uid map: denied' >&2; exit 1",
            false,
        ),
        ("gone", "exit 1", true),
    ] {
        let (dir, ws, home) = project(&format!("dflt-launcher-{tag}"));
        let path = path_with_fake_bwrap(&dir, body, chmod);
        let r = mission(&ws, &home, "", &path, "touch verify-ran");
        assert_ne!(r.code, 0, "{tag}\n{}\n{}", r.stdout, r.stderr);
        assert!(
            r.stdout.contains("status: verify_refused"),
            "{tag}: {}",
            r.stdout
        );
        assert!(
            !ws.join("verify-ran").exists(),
            "{tag}: never ran unsandboxed"
        );
        assert!(!r.stderr.contains("UNSANDBOXED"), "{tag}: {}", r.stderr);
        assert!(fallback_lines(&home, "verify").is_empty(), "{tag}");
        drop(dir);
    }
}

/// The stop gate under the default: sandboxed with bubblewrap...
#[test]
#[cfg(feature = "shell-tools")]
fn the_default_stop_gate_is_sandboxed_on_a_bwrap_host() {
    if !bwrap_works() {
        return;
    }
    let (dir, ws, home) = project("dflt-gate-bwrap");
    let gate = gate_script(
        &ws,
        "echo ok > gate-made; ! (echo x > .git/hooks/h) 2>/dev/null || { echo hooks writable; exit 1; }",
    );
    let r = run_with_gate(&ws, &home, "", &path_env(), &gate);
    assert_eq!(r.code, 0, "{}\n{}", r.stdout, r.stderr);
    assert!(ws.join("gate-made").exists() && !ws.join(".git/hooks/h").exists());
    drop(dir);
}

/// ...a loud unsandboxed run, ledgered, where the host has no backend...
#[test]
#[cfg(feature = "shell-tools")]
fn the_default_stop_gate_without_any_sandbox_runs_loudly_and_is_ledgered() {
    let (dir, ws, home) = project("dflt-gate-nosandbox");
    let gate = gate_script(&ws, "touch gate-ran; exit 0");
    let r = run_with_gate(&ws, &home, "", &path_without_bwrap(&dir), &gate);
    assert_eq!(r.code, 0, "{}\n{}", r.stdout, r.stderr);
    assert!(ws.join("gate-ran").exists(), "the gate ran, unsandboxed");
    assert!(
        r.stderr.contains("WARNING: turnend runs UNSANDBOXED")
            && r.stderr.contains("sandbox.consumers.turnend = \"off\""),
        "{}",
        r.stderr
    );
    assert_eq!(fallback_lines(&home, "turnend").len(), 1);
    drop(dir);
}

/// ...and a deny when the backend exists and its launcher fails.
#[test]
#[cfg(feature = "shell-tools")]
fn the_default_stop_gate_whose_launcher_fails_is_still_a_deny() {
    let (dir, ws, home) = project("dflt-gate-launcher");
    let gate = gate_script(&ws, "touch gate-ran; exit 0");
    let path = path_with_fake_bwrap(&dir, "echo 'bwrap: denied' >&2; exit 1", false);
    let r = run_with_gate(&ws, &home, "", &path, &gate);
    assert_eq!(r.code, 6, "{}\n{}", r.stdout, r.stderr);
    assert!(!ws.join("gate-ran").exists(), "never ran unsandboxed");
    assert!(!r.stderr.contains("UNSANDBOXED"), "{}", r.stderr);
    drop(dir);
}

// ---------------------------------------------------------------------------
// #748, security review: nothing but "no backend at all" may downgrade the
// default, and the backend lookup must not let a bad candidate decide.
// ---------------------------------------------------------------------------

/// A verify that passes only when the sandbox really protects `.git/hooks`.
const PROTECTED_VERIFY: &str = "echo ok > made && ! (echo x > .git/hooks/h) 2>/dev/null";

fn plant_script(path: &Path, mode: u32, body: &str) {
    use std::os::unix::fs::PermissionsExt;
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, body).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).unwrap();
}

/// A file named `bwrap` WITHOUT the execute bit earlier on PATH must not
/// shadow the real one behind it (the default used to run unsandboxed on
/// "cannot run bwrap: Permission denied").
#[test]
fn a_non_executable_bwrap_earlier_on_path_does_not_downgrade_the_default() {
    if !bwrap_works() {
        return;
    }
    let (dir, ws, home) = project("dflt-nonexec");
    let bad = dir.join("badbin");
    plant_script(&bad.join("bwrap"), 0o644, "#!/bin/sh\nexit 1\n");
    let path = format!("{}:{}", bad.display(), path_env());
    let r = mission(&ws, &home, "", &path, PROTECTED_VERIFY);
    assert_eq!(r.code, 0, "{}\n{}", r.stdout, r.stderr);
    assert!(r.stdout.contains("status: complete"), "{}", r.stdout);
    assert!(!r.stderr.contains("UNSANDBOXED"), "{}", r.stderr);
    assert!(ws.join("made").exists() && !ws.join(".git/hooks/h").exists());
    drop(dir);
}

/// A relative PATH entry (`.`) with a model-writable `./bwrap` that fails
/// must not decide what the sandbox is.
#[test]
fn a_relative_path_entry_with_a_hostile_bwrap_does_not_downgrade_the_default() {
    if !bwrap_works() {
        return;
    }
    let (dir, ws, home) = project("dflt-relpath");
    // Passes the probe, then runs the command with no sandbox at all.
    plant_script(
        &ws.join("bwrap"),
        0o755,
        "#!/bin/sh\ncase \"$1\" in --version) echo 'bubblewrap 0.9.0'; exit 0;; esac\n\
         for a in \"$@\"; do last=$a; done\n\
         if [ \"$last\" = true ]; then exit 0; fi\n\
         while [ \"$1\" != -- ]; do shift; done; shift; exec \"$@\"\n",
    );
    let path = format!(".:{}", path_env());
    let r = mission(&ws, &home, "", &path, PROTECTED_VERIFY);
    assert_eq!(r.code, 0, "{}\n{}", r.stdout, r.stderr);
    assert!(r.stdout.contains("status: complete"), "{}", r.stdout);
    assert!(!r.stderr.contains("UNSANDBOXED"), "{}", r.stderr);
    assert!(!ws.join(".git/hooks/h").exists(), "the sandbox applied");
    drop(dir);
}

/// The reason in the warning can hold a backend's stderr: it stays on one
/// line and carries no escape sequence.
#[test]
fn the_fallback_warning_is_one_sanitized_line() {
    let (dir, ws, home) = project("dflt-sanitize");
    let fake = dir.join("fakebin");
    plant_script(
        &fake.join("bwrap"),
        0o755,
        "#!/bin/sh\ncase \"$1\" in --version) echo 'bubblewrap 0.9.0'; exit 0;; esac\n\
         printf '\\033[31mEVIL\\033[0m\\nSECOND-LINE\\n' >&2\nexit 1\n",
    );
    let path = format!("{}:{}", fake.display(), path_without_bwrap(&dir));
    let r = mission(&ws, &home, "", &path, "touch verify-ran");
    assert_eq!(r.code, 0, "{}\n{}", r.stdout, r.stderr);
    assert!(
        ws.join("verify-ran").exists(),
        "no usable sandbox: degraded"
    );
    assert!(!r.stderr.contains('\x1b'), "{:?}", r.stderr);
    assert!(r.stderr.contains("WARNING: verify runs UNSANDBOXED"));
    assert!(
        r.stderr.lines().all(|l| !l.starts_with("SECOND-LINE")),
        "the reason must not start a line of its own: {}",
        r.stderr
    );
    drop(dir);
}

/// The backend is probed at start-up when a consumer is only optional by
/// default, before any command runs (serve, acp, mcp and the gates have no
/// start-up check of their own); not at all when both are written off.
#[test]
fn the_backend_is_probed_at_start_up_when_a_consumer_is_implicit() {
    let probes = |cfg: &str, tag: &str| -> usize {
        let (dir, _ws, home) = project(tag);
        let fake = dir.join("fakebin");
        let log = dir.join("calls");
        plant_script(
            &fake.join("bwrap"),
            0o755,
            &format!("#!/bin/sh\necho \"$1\" >> {}\nexit 1\n", log.display()),
        );
        std::fs::create_dir_all(home.join("tole")).unwrap();
        std::fs::write(home.join("tole/config.toml"), cfg).unwrap();
        let out = scrubbed_tole(&home)
            .args(["sessions", "-s"])
            .arg(home.join("sessions"))
            .current_dir(&dir)
            .env(
                "PATH",
                format!("{}:{}", fake.display(), path_without_bwrap(&dir)),
            )
            .env("TOLE_NO_UPDATE_CHECK", "1")
            .stdin(Stdio::null())
            .output()
            .expect("run tole");
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        std::fs::read_to_string(&log).map_or(0, |t| t.lines().count())
    };
    assert!(probes("", "probe-default") > 0, "probed at start-up");
    assert_eq!(
        probes(
            "[sandbox.consumers]\nverify = \"off\"\nturnend = \"off\"\n",
            "probe-off"
        ),
        0,
        "nothing to probe for"
    );
}
