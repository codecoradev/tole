//! #534 review gate — end to end with the REAL binary: a scripted provider
//! asks the `git` tool to commit; a fake `cora` serves the two review tools
//! over MCP (`cora mcp`) and answers `cora hook verify` from an exit-code
//! file; the repository is a real temp git repo with one staged file.

#![cfg(all(unix, feature = "mcp"))]

use serde_json::{json, Value};
use std::io::{Read, Write};
use std::net::TcpListener;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};

fn tmpdir(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!(
        "tole-gate-e2e-{tag}-{}-{:x}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&d).unwrap();
    d
}

/// Fake `cora`: `mcp` = a stdio MCP server listing the review tools;
/// `hook verify [--help|--quiet]` = exit code from the `exit` file beside it;
/// `--version` = a supported version.
fn fake_cora(dir: &Path, verify_exit: i32) -> PathBuf {
    let bin = dir.join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    std::fs::write(bin.join("exit"), verify_exit.to_string()).unwrap();
    let script = r#"#!/usr/bin/env python3
import json, os, sys
here = os.path.dirname(os.path.abspath(__file__))
a = sys.argv[1:]
if a[:2] == ["hook", "verify"]:
    if "--help" in a:
        sys.exit(0)
    sys.exit(int(open(os.path.join(here, "exit")).read().strip()))
if a[:1] == ["--version"]:
    print("cora 0.20.0"); sys.exit(0)
if a[:1] != ["mcp"]:
    sys.exit(64)
def reply(i, result=None, error=None):
    m = {"jsonrpc": "2.0", "id": i}
    m["error" if error else "result"] = error or result
    sys.stdout.write(json.dumps(m) + "\n"); sys.stdout.flush()
for line in sys.stdin:
    try:
        r = json.loads(line)
    except Exception:
        continue
    i, m = r.get("id"), r.get("method")
    if i is None:
        continue
    if m == "initialize":
        reply(i, {"protocolVersion": r["params"]["protocolVersion"],
                  "capabilities": {"tools": {}}, "serverInfo": {"name": "cora", "version": "0"}})
    elif m == "tools/list":
        s = {"type": "object", "properties": {"diff": {"type": "string"}}}
        reply(i, {"tools": [
            {"name": "cora_review_prepare", "description": "prepare", "inputSchema": s},
            {"name": "cora_review_finalize", "description": "finalize", "inputSchema": s}]})
    else:
        reply(i, error={"code": -32601, "message": "method not found"})
"#;
    let p = bin.join("cora");
    std::fs::write(&p, script).unwrap();
    std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
    bin
}

/// #599: the owner's approval of the fake `cora` server (its exact tool
/// manifest and version) in the user plugin lock of `home`, so the guard
/// lets it register in an unattended run.
fn approve_fake_cora(home: &std::path::Path) {
    use tole_core::plugin_lock::{manifest_hash, LockEntry, Lockfile, ManifestTool, LOCK_FILE};
    let s = json!({"type": "object", "properties": {"diff": {"type": "string"}}});
    let tools: Vec<ManifestTool> = ["prepare", "finalize"]
        .iter()
        .map(|d| ManifestTool {
            name: format!("cora_review_{d}"),
            description: d.to_string(),
            input_schema: s.clone(),
        })
        .collect();
    let entry = LockEntry {
        source: "stdio:cora mcp".into(),
        version: "0".into(),
        manifest_hash: manifest_hash(&tools),
        ..Default::default()
    };
    let lock = Lockfile {
        servers: [("cora".to_string(), entry)].into(),
        ..Default::default()
    };
    lock.save(&home.join("tole").join(LOCK_FILE)).unwrap();
}

fn set_verify(bin: &Path, code: i32) {
    std::fs::write(bin.join("exit"), code.to_string()).unwrap();
}

/// A repo with one staged file. Returns its path.
fn repo(dir: &Path) -> PathBuf {
    let r = dir.join("repo");
    std::fs::create_dir_all(&r).unwrap();
    for args in [
        vec!["init", "-q"],
        vec!["config", "user.email", "t@example.com"],
        vec!["config", "user.name", "t"],
        vec!["config", "commit.gpgsign", "false"],
    ] {
        assert!(Command::new("git")
            .args(&args)
            .current_dir(&r)
            .status()
            .unwrap()
            .success());
    }
    std::fs::write(r.join("a.txt"), "hello\n").unwrap();
    assert!(Command::new("git")
        .args(["add", "a.txt"])
        .current_dir(&r)
        .status()
        .unwrap()
        .success());
    r
}

fn commit_count(repo: &Path) -> usize {
    let o = Command::new("git")
        .args(["rev-list", "--count", "HEAD"])
        .current_dir(repo)
        .output()
        .unwrap();
    if !o.status.success() {
        return 0; // no commits yet
    }
    String::from_utf8_lossy(&o.stdout)
        .trim()
        .parse()
        .unwrap_or(0)
}

/// Scripted provider: the first request asks the `git` tool to commit, the
/// next one (after the tool result) answers. Every request body is kept.
fn spawn_provider(tool: &str, args: &str) -> (String, Arc<Mutex<Vec<Value>>>) {
    let (tool, args) = (tool.to_string(), args.to_string());
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let bodies: Arc<Mutex<Vec<Value>>> = Arc::default();
    let seen = Arc::clone(&bodies);
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let seen = Arc::clone(&seen);
            let (tool, args) = (tool.clone(), args.clone());
            std::thread::spawn(move || {
                let mut s = stream;
                let mut buf = Vec::new();
                let mut chunk = [0u8; 16384];
                let mut body = Value::Null;
                let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
                while std::time::Instant::now() < deadline {
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
                let streamed = body.get("stream").and_then(Value::as_bool) == Some(true);
                let msgs = body["messages"].as_array().cloned().unwrap_or_default();
                let has_tool_result = msgs.iter().any(|m| m["role"] == "tool");
                seen.lock().unwrap().push(body);
                let (content, calls) = if has_tool_result {
                    (Some("done"), None)
                } else {
                    (
                        None,
                        Some(json!([{"id": "call_1", "type": "function", "function": {
                            "name": tool, "arguments": args}}])),
                    )
                };
                let resp = if streamed {
                    let mut out = String::new();
                    if let Some(c) = content {
                        out.push_str(&format!(
                            "data: {}\n\n",
                            json!({"choices": [{"index": 0, "delta": {"content": c},
                                   "finish_reason": null}]})
                        ));
                    }
                    if let Some(tcs) = &calls {
                        out.push_str(&format!(
                            "data: {}\n\n",
                            json!({"choices": [{"index": 0, "delta": {"tool_calls": [
                                {"index": 0, "id": tcs[0]["id"], "function": {
                                    "name": tcs[0]["function"]["name"],
                                    "arguments": tcs[0]["function"]["arguments"]}}]},
                                "finish_reason": null}]})
                        ));
                    }
                    out.push_str(&format!(
                        "data: {}\n\ndata: [DONE]\n\n",
                        json!({"choices": [], "usage": {"prompt_tokens": 1,
                               "completion_tokens": 1, "total_tokens": 2}})
                    ));
                    format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                        out.len(), out
                    )
                } else {
                    let data = json!({
                        "id": "c", "object": "chat.completion", "created": 1, "model": "mock",
                        "choices": [{"index": 0, "message": {"role": "assistant",
                            "content": content, "tool_calls": calls},
                            "finish_reason": if content.is_some() { "stop" } else { "tool_calls" }}],
                        "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2}
                    })
                    .to_string();
                    format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                        data.len(), data
                    )
                };
                let _ = s.write_all(resp.as_bytes());
            });
        }
    });
    (format!("http://{addr}/v1"), bodies)
}

struct Out {
    stderr: String,
    code: i32,
    /// The text of the tool result the model received, if any.
    tool_result: Option<String>,
}

/// `tole run --yes "commit it"` in `repo`, with the fake cora first on PATH;
/// the scripted model asks the `git` tool to commit.
fn run_tole(repo: &Path, bin: &Path, extra: &[&str], env: &[(&str, &str)]) -> Out {
    run_tole_with(
        repo,
        bin,
        extra,
        env,
        "git",
        r#"{"op":"commit","message":"add a"}"#,
    )
}

fn run_tole_with(
    repo: &Path,
    bin: &Path,
    extra: &[&str],
    env: &[(&str, &str)],
    tool: &str,
    args: &str,
) -> Out {
    let (base, bodies) = spawn_provider(tool, args);
    let home = tmpdir("home");
    approve_fake_cora(&home);
    let sessions = home.join("sessions");
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_tole"));
    cmd.args(["run", "--yes", "-s", sessions.to_str().unwrap()])
        .args(extra)
        .arg("commit it")
        .current_dir(repo)
        .stdin(Stdio::null())
        .env("PATH", format!("{}:/usr/bin:/bin", bin.display()))
        .env("HOME", &home)
        .env("CODECORA_HOME", &home)
        .env("TOLE_BASE_URL", base)
        .env("TOLE_MODEL", "mock-model")
        .env("TOLE_API_KEY", "sk-test")
        .env("TOLE_NO_UPDATE_CHECK", "1")
        .env_remove("TOLE_REVIEW_GATE")
        .env_remove("TOLE_STREAM")
        .env_remove("TOLE_MEMORY");
    for (k, v) in env {
        cmd.env(k, v);
    }
    let o = cmd.output().unwrap();
    let tool_result = bodies
        .lock()
        .unwrap()
        .iter()
        .flat_map(|b| b["messages"].as_array().cloned().unwrap_or_default())
        .filter(|m| m["role"] == "tool")
        .map(|m| m["content"].to_string())
        .next_back();
    Out {
        stderr: String::from_utf8_lossy(&o.stderr).to_string(),
        code: o.status.code().unwrap_or(-1),
        tool_result,
    }
}

#[test]
fn an_unreviewed_staged_diff_cannot_be_committed_and_the_model_is_told_why() {
    let dir = tmpdir("block");
    let bin = fake_cora(&dir, 11);
    let r = repo(&dir);
    let out = run_tole(&r, &bin, &[], &[]);
    assert_eq!(out.code, 0, "{}", out.stderr);
    assert_eq!(commit_count(&r), 0, "the commit must not have happened");
    let said = out.tool_result.expect("the model received a tool result");
    assert!(said.contains("review gate"), "{said}");
    assert!(said.contains("mcp_cora_cora_review_finalize"), "{said}");
}

#[test]
fn once_the_staged_diff_is_validated_the_commit_goes_through() {
    let dir = tmpdir("pass");
    let bin = fake_cora(&dir, 11);
    let r = repo(&dir);
    assert_eq!(run_tole(&r, &bin, &[], &[]).code, 0);
    assert_eq!(commit_count(&r), 0);
    // The review happens (cora records its marker): verify now says validated.
    set_verify(&bin, 0);
    let out = run_tole(&r, &bin, &[], &[]);
    assert_eq!(out.code, 0, "{}", out.stderr);
    assert_eq!(commit_count(&r), 1);
}

#[test]
fn a_blocking_verdict_refuses_the_commit_too() {
    let dir = tmpdir("blocking");
    let bin = fake_cora(&dir, 10);
    let r = repo(&dir);
    let out = run_tole(&r, &bin, &[], &[]);
    assert_eq!(commit_count(&r), 0);
    assert!(out.tool_result.unwrap().contains("BLOCKS"));
}

#[test]
fn off_and_warn_do_not_refuse_and_the_flag_beats_the_env() {
    let dir = tmpdir("policy");
    let bin = fake_cora(&dir, 11);
    let r = repo(&dir);
    // --review-gate off beats TOLE_REVIEW_GATE=block.
    let out = run_tole(
        &r,
        &bin,
        &["--review-gate", "off"],
        &[("TOLE_REVIEW_GATE", "block")],
    );
    assert_eq!(out.code, 0, "{}", out.stderr);
    assert_eq!(commit_count(&r), 1, "off: committed");

    let r2 = repo(&tmpdir("policy2"));
    let out = run_tole(&r2, &bin, &[], &[("TOLE_REVIEW_GATE", "warn")]);
    assert_eq!(commit_count(&r2), 1, "warn: committed");
    assert!(out.tool_result.unwrap().contains("would be refused"));
}

#[test]
fn without_the_review_tools_there_is_no_gate() {
    // --no-auto-mcp: cora's tools are not attached, so the agent could not
    // get a review; gating would only lock it out.
    let dir = tmpdir("nomcp");
    let bin = fake_cora(&dir, 11);
    let r = repo(&dir);
    let out = run_tole(&r, &bin, &["--no-auto-mcp"], &[]);
    assert_eq!(out.code, 0, "{}", out.stderr);
    assert_eq!(commit_count(&r), 1);
}

#[test]
fn an_invalid_policy_fails_startup_loudly() {
    let dir = tmpdir("junk");
    let bin = fake_cora(&dir, 11);
    let r = repo(&dir);
    let out = run_tole(&r, &bin, &[], &[("TOLE_REVIEW_GATE", "maybe")]);
    assert_ne!(out.code, 0);
    assert!(out.stderr.contains("TOLE_REVIEW_GATE"), "{}", out.stderr);
    assert_eq!(commit_count(&r), 0);
}

#[test]
fn doctor_reports_the_gate() {
    let dir = tmpdir("doctor");
    let bin = fake_cora(&dir, 0);
    let home = tmpdir("home");
    let o = Command::new(env!("CARGO_BIN_EXE_tole"))
        .args(["doctor", "--tier", "offline", "--no-ledger"])
        .env("PATH", format!("{}:/usr/bin:/bin", bin.display()))
        .env("CODECORA_HOME", &home)
        .env("TOLE_NO_UPDATE_CHECK", "1")
        .env_remove("TOLE_REVIEW_GATE")
        .output()
        .unwrap();
    let out = String::from_utf8_lossy(&o.stdout);
    assert!(out.contains("review gate: block"), "{out}");
}

/// The dedicated `git` tool is not the only way to commit: `run_command` and
/// `job_start` run `git commit` as an argv too, so they are gated the same.
#[test]
fn run_command_and_job_start_cannot_be_used_to_get_around_the_gate() {
    for tool in ["run_command", "job_start"] {
        let dir = tmpdir(tool);
        let bin = fake_cora(&dir, 11);
        let r = repo(&dir);
        let out = run_tole_with(
            &r,
            &bin,
            &[],
            &[],
            tool,
            r#"{"command":"git -C . commit --no-verify -m x"}"#,
        );
        assert_eq!(out.code, 0, "{tool}: {}", out.stderr);
        assert_eq!(
            commit_count(&r),
            0,
            "{tool}: the commit must not have happened"
        );
        assert!(out.tool_result.unwrap().contains("review gate"), "{tool}");
    }
    // Control: with the gate off the same call commits (so the refusal above
    // is the gate, not a malformed command).
    let dir = tmpdir("ctl");
    let bin = fake_cora(&dir, 11);
    let r = repo(&dir);
    let out = run_tole_with(
        &r,
        &bin,
        &["--review-gate", "off"],
        &[],
        "run_command",
        r#"{"command":"git commit -m x"}"#,
    );
    assert_eq!(out.code, 0, "{}", out.stderr);
    assert_eq!(commit_count(&r), 1);
}
