//! #208 part 3b end-to-end with the REAL binary: the security-sensitive
//! config keys (`trust`, `allow`, `mcp_server`, hooks, `skill`, `plan_mode`,
//! `no_auto_mcp`, `no_skills`, `[mission]` verify/verify_timeout) are
//! applied after trust, refused on the faces that refuse the flags, and
//! can never loosen the Destructive rule.
//!
//! Hermetic: `CODECORA_HOME` and every provider variable are set on the
//! CHILD only, the provider is a local scripted mock, and `PATH` is a
//! private directory plus `/usr/bin:/bin` (no `cora` auto-preset unless a
//! test installs a fake one).

use serde_json::{json, Value};
use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

static COUNTER: AtomicU64 = AtomicU64::new(0);

fn tmpdir() -> PathBuf {
    let n = COUNTER.fetch_add(1, Ordering::SeqCst);
    let d = std::env::temp_dir().join(format!("tole-sens-e2e-{}-{n}", std::process::id()));
    std::fs::create_dir_all(&d).unwrap();
    d.canonicalize().unwrap()
}

const SCRUBBED: [&str; 11] = [
    "TOLE_MODEL",
    "TOLE_BASE_URL",
    "TOLE_API_KEY",
    "TOLE_MEMORY",
    "TOLE_SYSTEM_PROMPT",
    "TOLE_TRUST",
    "OPENAI_MODEL",
    "OPENAI_BASE_URL",
    "OPENAI_API_KEY",
    "TOLE_SERVE_TOKEN",
    "TOLE_MEMORY_NAMESPACE",
];

fn text(b: &[u8]) -> String {
    String::from_utf8_lossy(b).to_string()
}

/// [`project_raw`], minus the `no_auto_mcp` line in a build without the
/// `mcp` feature (there the key is a startup error, tested separately):
/// lets the same scenario run in every feature profile.
fn project(content: &str) -> (PathBuf, PathBuf) {
    if cfg!(feature = "mcp") {
        project_raw(content)
    } else {
        project_raw(&content.replace("no_auto_mcp = true\n", ""))
    }
}

/// A project dir with `.tole/config.toml` = `content`, a fresh home, and the
/// config already trusted.
fn project_raw(content: &str) -> (PathBuf, PathBuf) {
    let d = tmpdir();
    std::fs::create_dir_all(d.join(".tole")).unwrap();
    std::fs::write(d.join(".tole/config.toml"), content).unwrap();
    let home = tmpdir();
    let o = base_cmd(&d, &home, &["config", "trust", "--yes"])
        .output()
        .unwrap();
    assert!(o.status.success(), "{}", text(&o.stderr));
    (d, home)
}

fn base_cmd(cwd: &Path, home: &Path, args: &[&str]) -> Command {
    let mut c = Command::new(env!("CARGO_BIN_EXE_tole"));
    c.args(args)
        .current_dir(cwd)
        .env("TOLE_NO_UPDATE_CHECK", "1")
        .env("CODECORA_HOME", home)
        // No cora / uteke / gh on the path: nothing auto-attaches.
        .env("PATH", format!("{}:/usr/bin:/bin", tmpdir().display()))
        .stdin(Stdio::null());
    for k in SCRUBBED {
        c.env_remove(k);
    }
    c
}

/// Run to completion; a hung child is a test failure, not a hang.
fn finish(mut c: Command) -> Output {
    c.stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = c.spawn().unwrap();
    let start = Instant::now();
    loop {
        if child.try_wait().unwrap().is_some() {
            return child.wait_with_output().unwrap();
        }
        if start.elapsed() > Duration::from_secs(60) {
            let _ = child.kill();
            panic!("child did not finish in 60s");
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

// ----------------------------------------------------------------- mock

#[derive(Clone)]
enum Step {
    Final(&'static str),
    Tool(&'static str, &'static str),
}

type Seen = Arc<Mutex<Vec<Value>>>;

/// Scripted provider: request n answers `steps[min(n, last)]`; every
/// request body is recorded.
fn spawn_mock(steps: Vec<Step>) -> (String, Seen) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let seen: Seen = Arc::default();
    let counter = Arc::new(AtomicUsize::new(0));
    let (seen2, steps) = (Arc::clone(&seen), Arc::new(steps));
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let (seen, counter, steps) =
                (Arc::clone(&seen2), Arc::clone(&counter), Arc::clone(&steps));
            std::thread::spawn(move || {
                let mut s = stream;
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
                let streamed = body["stream"].as_bool() == Some(true);
                seen.lock().unwrap().push(body);
                let n = counter.fetch_add(1, Ordering::SeqCst);
                let step = steps[n.min(steps.len() - 1)].clone();
                let (content, calls) = match step {
                    Step::Final(t) => (Some(t.to_string()), None),
                    Step::Tool(name, args) => (
                        None,
                        Some(json!([{"id": "call_1", "type": "function",
                            "function": {"name": name, "arguments": args}}])),
                    ),
                };
                let resp = if streamed {
                    let mut out = String::new();
                    if let Some(c) = &content {
                        out.push_str(&format!(
                            "data: {}\n\n",
                            json!({"choices": [{"index": 0,
                                "delta": {"content": c}, "finish_reason": null}]})
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
                        out.len(),
                        out
                    )
                } else {
                    let mut msg = json!({"role": "assistant", "content": content});
                    if let Some(c) = calls {
                        msg["tool_calls"] = c;
                    }
                    let data = json!({
                        "id": "c", "object": "chat.completion", "created": 1, "model": "mock",
                        "choices": [{"index": 0, "message": msg,
                            "finish_reason": if content.is_some() { "stop" } else { "tool_calls" }}],
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
    (format!("http://{addr}/v1"), seen)
}

fn with_env(mut c: Command, k: &str, v: &str) -> Command {
    c.env(k, v);
    c
}

/// A provider-wired command (the key comes from the environment only).
fn with_provider(mut c: Command, url: &str) -> Command {
    c.env("TOLE_BASE_URL", url)
        .env("TOLE_MODEL", "mock")
        .env("TOLE_API_KEY", "k");
    c
}

fn tool_names(body: &Value) -> Vec<String> {
    body["tools"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|t| {
                    t["function"]["name"]
                        .as_str()
                        .or_else(|| t["name"].as_str())
                        .map(str::to_string)
                })
                .collect()
        })
        .unwrap_or_default()
}

fn system_text(body: &Value) -> String {
    body["messages"][0]["content"]
        .as_str()
        .unwrap_or_default()
        .to_string()
}

// ------------------------------------------------------------ plan_mode

#[test]
fn plan_mode_from_config_has_the_effect_of_the_flag() {
    let (url, seen) = spawn_mock(vec![Step::Final("planned")]);
    let (d, home) = project("plan_mode = true\n");
    let o = finish(with_provider(base_cmd(&d, &home, &["run", "hi"]), &url));
    assert!(o.status.success(), "{}", text(&o.stderr));
    let body = seen.lock().unwrap()[0].clone();
    let names = tool_names(&body);
    assert!(names.iter().any(|n| n == "read_file"), "{names:?}");
    for banned in ["write_file", "edit_file", "delete_file", "run_command"] {
        assert!(
            !names.iter().any(|n| n == banned),
            "{banned} on the wire: {names:?}"
        );
    }
    assert!(
        system_text(&body).contains("PLAN MODE"),
        "{}",
        system_text(&body)
    );
    // The same flag gives the same tool set (parity).
    let (url2, seen2) = spawn_mock(vec![Step::Final("planned")]);
    let o = finish(with_provider(
        base_cmd(&d, &home, &["--no-config", "--plan-mode", "run", "hi"]),
        &url2,
    ));
    assert!(o.status.success(), "{}", text(&o.stderr));
    let flag_names = tool_names(&seen2.lock().unwrap()[0]);
    let mut a = names;
    let mut b = flag_names;
    a.sort();
    b.sort();
    assert_eq!(a, b);
}

#[test]
fn without_the_key_write_tools_are_on_the_wire_and_no_config_drops_it() {
    let (url, seen) = spawn_mock(vec![Step::Final("ok")]);
    let (d, home) = project("plan_mode = true\n");
    let o = finish(with_provider(
        base_cmd(&d, &home, &["--no-config", "run", "hi"]),
        &url,
    ));
    assert!(o.status.success(), "{}", text(&o.stderr));
    let names = tool_names(&seen.lock().unwrap()[0]);
    assert!(names.iter().any(|n| n == "write_file"), "{names:?}");
    assert!(!system_text(&seen.lock().unwrap()[0]).contains("PLAN MODE"));
}

// ---------------------------------------------------------------- allow

#[test]
fn allow_from_config_runs_a_listed_write_tool_without_a_prompt() {
    let (url, _) = spawn_mock(vec![
        Step::Tool("write_file", r#"{"path":"out.txt","content":"auto"}"#),
        Step::Final("done"),
    ]);
    let (d, home) = project("allow = [\"write_*\"]\n");
    let o = finish(with_provider(base_cmd(&d, &home, &["run", "go"]), &url));
    assert!(o.status.success(), "{}", text(&o.stderr));
    assert_eq!(std::fs::read_to_string(d.join("out.txt")).unwrap(), "auto");
}

#[test]
fn a_tool_not_in_the_config_allow_list_is_denied() {
    let (url, _) = spawn_mock(vec![
        Step::Tool("write_file", r#"{"path":"out.txt","content":"x"}"#),
        Step::Final("done"),
    ]);
    // stdin is /dev/null: the prompt reads EOF = deny.
    let (d, home) = project("allow = [\"edit_file\"]\n");
    let o = finish(with_provider(base_cmd(&d, &home, &["run", "go"]), &url));
    assert!(!o.status.success());
    assert!(
        text(&o.stderr).contains("approval denied for 'write_file'"),
        "{}",
        text(&o.stderr)
    );
    assert!(!d.join("out.txt").exists());
}

#[test]
fn a_flag_allow_list_replaces_the_config_list_wholesale() {
    let (url, _) = spawn_mock(vec![
        Step::Tool("write_file", r#"{"path":"out.txt","content":"x"}"#),
        Step::Final("done"),
    ]);
    // Config would allow write_file; the (non-empty) flag list replaces it.
    let (d, home) = project("allow = [\"write_*\"]\n");
    let o = finish(with_provider(
        base_cmd(&d, &home, &["run", "--allow", "edit_file", "go"]),
        &url,
    ));
    assert!(
        !o.status.success(),
        "flag list must replace the config list"
    );
    assert!(!d.join("out.txt").exists());
}

// ---------------------------------------------------------------- trust

#[test]
fn trust_from_config_expands_the_preset_and_a_flag_replaces_it() {
    let todo = r#"{"todos":[{"content":"x","status":"pending"}]}"#;
    // internal => todo_write is auto-allowed (no prompt, no denial).
    let (url, _) = spawn_mock(vec![Step::Tool("todo_write", todo), Step::Final("trusted")]);
    let (d, home) = project("trust = [\"internal\"]\n");
    let o = finish(with_provider(base_cmd(&d, &home, &["run", "go"]), &url));
    assert!(o.status.success(), "{}", text(&o.stderr));
    assert!(text(&o.stdout).contains("trusted"), "{}", text(&o.stdout));
    // `--trust none` (a non-empty flag list) replaces the config presets.
    let (url, _) = spawn_mock(vec![Step::Tool("todo_write", todo), Step::Final("trusted")]);
    let o = finish(with_provider(
        base_cmd(&d, &home, &["--trust", "none", "run", "go"]),
        &url,
    ));
    assert!(!o.status.success());
    assert!(text(&o.stderr).contains("approval denied for 'todo_write'"));
    // The env layer sits between flag and config.
    let (url, _) = spawn_mock(vec![Step::Tool("todo_write", todo), Step::Final("trusted")]);
    let o = finish(with_provider(
        with_env(base_cmd(&d, &home, &["run", "go"]), "TOLE_TRUST", "none"),
        &url,
    ));
    assert!(!o.status.success(), "TOLE_TRUST must beat the config trust");
}

// ---------------------------------------------------------- Destructive

/// SECURITY: a config `allow` / `trust` that matches a Destructive tool
/// must never skip its prompt — exactly as with the flag. The prompt reads
/// EOF on /dev/null and denies, so the file survives.
#[test]
fn destructive_tools_cannot_be_allowed_via_config_allow_or_trust() {
    for config in [
        "allow = [\"delete_file\"]\n",
        "allow = [\"*\"]\n",
        "allow = [\"delete_*\", \"*\"]\ntrust = [\"internal\", \"read_only\"]\n",
    ] {
        let (url, _) = spawn_mock(vec![
            Step::Tool("delete_file", r#"{"path":"victim.txt"}"#),
            Step::Final("deleted"),
        ]);
        let (d, home) = project(config);
        std::fs::write(d.join("victim.txt"), "precious").unwrap();
        let o = finish(with_provider(base_cmd(&d, &home, &["run", "go"]), &url));
        assert!(!o.status.success(), "{config}: {}", text(&o.stdout));
        assert!(
            text(&o.stderr).contains("approval denied for 'delete_file'"),
            "{config}: {}",
            text(&o.stderr)
        );
        assert_eq!(
            std::fs::read_to_string(d.join("victim.txt")).unwrap(),
            "precious",
            "{config}: the destructive tool executed"
        );
    }
}

/// The same machinery, one level down: patterns built from a CONFIG list
/// still hit the Destructive wall in the approver and the registry.
#[test]
fn config_patterns_hit_the_destructive_wall_in_approver_and_registry() {
    use tole_cli::approver::{InteractiveApprover, PromptFn};
    use tole_cli::config::parse;
    use tole_cli::config_apply::{values, Flags, Settings};
    use tole_core::approval::{AllowlistApprover, Approver, ToolRequest, Verdict};
    use tole_core::tool::{Risk, Tool, ToolRegistry};

    struct Deny;
    impl PromptFn for Deny {
        fn prompt(&self, _: &ToolRequest<'_>) -> Verdict {
            Verdict::Deny
        }
    }
    struct Bomb;
    impl Tool for Bomb {
        fn name(&self) -> &str {
            "delete_file"
        }
        fn risk(&self) -> Risk {
            Risk::Destructive
        }
        fn execute(&self, _: Value) -> Result<Value, String> {
            Ok(json!({}))
        }
    }

    let cfg = parse(
        "allow = [\"delete_file\", \"*\"]\ntrust = [\"internal\"]\n",
        Path::new("c.toml"),
    )
    .unwrap();
    let st = Settings::resolve(&Flags::default(), &cfg, &|_| None);
    let patterns = values(&st.allow);
    assert_eq!(patterns, ["delete_file", "*"]);
    let interactive = InteractiveApprover::new(Deny)
        .with_allow_patterns(patterns.clone())
        .with_auto_write(true);
    let req = ToolRequest {
        tool: "delete_file",
        risk: Risk::Destructive,
        description: String::new(),
        input: &json!({}),
    };
    assert_eq!(
        interactive.decide(&req),
        Verdict::Deny,
        "must reach the prompt"
    );
    // A non-interactive allowlist (the serve/acp/mcp machinery) refuses to
    // even REGISTER the Destructive tool.
    let mut reg = ToolRegistry::with_approver(AllowlistApprover::new(
        patterns,
        tole_core::approval::Decision::Allow,
    ));
    assert!(reg.register(Box::new(Bomb)).is_err());
}

// ---------------------------------------------------------------- hooks

#[cfg(unix)]
#[allow(dead_code)] // only the shell-tools / mcp tests use it
fn write_script(dir: &Path, name: &str, body: &str) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let p = dir.join(name);
    std::fs::write(&p, format!("#!/bin/sh\n{body}\n")).unwrap();
    std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
    p
}

#[cfg(all(unix, feature = "shell-tools"))]
#[test]
fn on_pretool_from_config_runs_and_can_deny() {
    let (url, _) = spawn_mock(vec![
        Step::Tool("write_file", r#"{"path":"out.txt","content":"x"}"#),
        Step::Final("done"),
    ]);
    let scripts = tmpdir();
    let marker = scripts.join("marker");
    let hook = write_script(
        &scripts,
        "deny.sh",
        "cat > \"$1.stdin\"\ntouch \"$1\"\nexit 2",
    );
    let (d, home) = project(&format!(
        "allow = [\"write_*\"]\non_pretool = [\"{} {}\"]\n",
        hook.display(),
        marker.display()
    ));
    let o = finish(with_provider(base_cmd(&d, &home, &["run", "go"]), &url));
    assert!(
        marker.exists(),
        "the config hook was not invoked: {}",
        text(&o.stderr)
    );
    let payload = std::fs::read_to_string(scripts.join("marker.stdin")).unwrap();
    assert!(
        payload.contains("\"pretool\"") && payload.contains("write_file"),
        "{payload}"
    );
    assert!(
        !d.join("out.txt").exists(),
        "a denying hook must stop the write"
    );
}

#[cfg(all(unix, feature = "shell-tools"))]
#[test]
fn on_posttool_and_on_turnend_from_config_run() {
    let (url, _) = spawn_mock(vec![
        Step::Tool("write_file", r#"{"path":"out.txt","content":"x"}"#),
        Step::Final("done"),
    ]);
    let scripts = tmpdir();
    let (post, end) = (scripts.join("post"), scripts.join("end"));
    let post_hook = write_script(&scripts, "post.sh", "cat > /dev/null\ntouch \"$1\"");
    let end_hook = write_script(&scripts, "end.sh", "cat > /dev/null\ntouch \"$1\"\nexit 0");
    let (d, home) = project(&format!(
        "allow = [\"write_*\"]\non_posttool = [\"{} {}\"]\non_turnend = [\"{} {}\"]\n",
        post_hook.display(),
        post.display(),
        end_hook.display(),
        end.display()
    ));
    let o = finish(with_provider(base_cmd(&d, &home, &["run", "go"]), &url));
    assert!(o.status.success(), "{}", text(&o.stderr));
    assert!(post.exists(), "posttool hook not run");
    assert!(end.exists(), "turnend hook not run");
}

// ----------------------------------------------------------- mcp_server

#[cfg(all(unix, feature = "mcp"))]
#[test]
fn mcp_server_from_config_is_attempted_and_the_flag_replaces_the_list() {
    let (url, _) = spawn_mock(vec![Step::Final("ok")]);
    let scripts = tmpdir();
    let (m_cfg, m_flag) = (scripts.join("cfg-spawned"), scripts.join("flag-spawned"));
    let stub = write_script(&scripts, "stub.sh", "touch \"$1\"\nexit 1");
    let (d, home) = project(&format!(
        "mcp_server = [\"cfgsrv={} {}\"]\n",
        stub.display(),
        m_cfg.display()
    ));
    let o = finish(with_provider(base_cmd(&d, &home, &["run", "hi"]), &url));
    assert!(o.status.success(), "{}", text(&o.stderr));
    assert!(m_cfg.exists(), "config mcp server never spawned");
    assert!(
        text(&o.stderr).contains("mcp[cfgsrv]"),
        "{}",
        text(&o.stderr)
    );
    // A non-empty flag list replaces the config list wholesale.
    std::fs::remove_file(&m_cfg).unwrap();
    let flag_spec = format!("flagsrv={} {}", stub.display(), m_flag.display());
    let o = finish(with_provider(
        base_cmd(&d, &home, &["--mcp-server", &flag_spec, "run", "hi"]),
        &url,
    ));
    assert!(o.status.success(), "{}", text(&o.stderr));
    assert!(m_flag.exists(), "flag mcp server never spawned");
    assert!(!m_cfg.exists(), "config list must be replaced, not merged");
}

#[cfg(all(unix, feature = "mcp"))]
#[test]
fn no_auto_mcp_from_config_suppresses_the_cora_preset() {
    let (url, _) = spawn_mock(vec![Step::Final("ok")]);
    let (d, home) = project("no_auto_mcp = true\n");
    let fake = tmpdir();
    let marker = fake.join("cora-ran");
    write_script(
        &fake,
        "cora",
        &format!("touch {}\nexit 1", marker.display()),
    );
    let with_fake_cora = |args: &[&str]| {
        let mut c = with_provider(base_cmd(&d, &home, args), &url);
        c.env("PATH", format!("{}:/usr/bin:/bin", fake.display()));
        c
    };
    let o = finish(with_fake_cora(&["run", "hi"]));
    assert!(o.status.success(), "{}", text(&o.stderr));
    assert!(
        !marker.exists(),
        "no_auto_mcp = true must stop the cora preset"
    );
    // Control: without the key (--no-config) the preset spawns the fake cora.
    let o = finish(with_fake_cora(&["--no-config", "run", "hi"]));
    assert!(o.status.success(), "{}", text(&o.stderr));
    assert!(
        marker.exists(),
        "control: the preset should have been attempted"
    );
}

// ------------------------------------------------------- skill / no_skills

fn skill_project(config: &str) -> (PathBuf, PathBuf) {
    let (d, home) = project(config);
    std::fs::create_dir_all(d.join("skills/demo")).unwrap();
    std::fs::write(
        d.join("skills/demo/SKILL.md"),
        "---\nname: demo\ndescription: a demo skill\n---\nDEMO-SKILL-BODY\n",
    )
    .unwrap();
    (d, home)
}

#[test]
fn skill_from_config_loads_into_the_system_prompt_and_flag_replaces_it() {
    let (url, seen) = spawn_mock(vec![Step::Final("ok")]);
    let (d, home) = skill_project("skill = [\"skills/demo/SKILL.md\"]\nno_auto_mcp = true\n");
    // A second skill that only the flag names.
    std::fs::create_dir_all(d.join("other/alt")).unwrap();
    std::fs::write(
        d.join("other/alt/SKILL.md"),
        "---\nname: alt\ndescription: alt\n---\nALT-SKILL-BODY\n",
    )
    .unwrap();
    let o = finish(with_provider(base_cmd(&d, &home, &["run", "hi"]), &url));
    assert!(o.status.success(), "{}", text(&o.stderr));
    assert!(system_text(&seen.lock().unwrap()[0]).contains("DEMO-SKILL-BODY"));
    let o = finish(with_provider(
        base_cmd(&d, &home, &["--skill", "other/alt/SKILL.md", "run", "hi"]),
        &url,
    ));
    assert!(o.status.success(), "{}", text(&o.stderr));
    let sys = system_text(&seen.lock().unwrap()[1]);
    assert!(sys.contains("ALT-SKILL-BODY"), "{sys}");
    assert!(
        !sys.contains("DEMO-SKILL-BODY"),
        "flag list must replace the config list"
    );
}

#[test]
fn no_skills_from_config_disables_discovery_and_load_skill() {
    let (url, seen) = spawn_mock(vec![Step::Final("ok")]);
    let (d, home) = skill_project("no_skills = true\nno_auto_mcp = true\n");
    // Discovery finds ./skills/demo only when skills are enabled.
    let o = finish(with_provider(base_cmd(&d, &home, &["run", "hi"]), &url));
    assert!(o.status.success(), "{}", text(&o.stderr));
    let names = tool_names(&seen.lock().unwrap()[0]);
    assert!(!names.iter().any(|n| n == "load_skill"), "{names:?}");
    let o = finish(with_provider(
        base_cmd(&d, &home, &["--no-config", "run", "hi"]),
        &url,
    ));
    assert!(o.status.success(), "{}", text(&o.stderr));
    let names = tool_names(&seen.lock().unwrap()[1]);
    assert!(
        names.iter().any(|n| n == "load_skill"),
        "control: {names:?}"
    );
}

// -------------------------------------------------------------- mission

#[test]
fn mission_verify_from_config_runs_and_the_flag_overrides_it() {
    let (url, _) = spawn_mock(vec![Step::Final("all done\nMISSION_COMPLETE")]);
    let scripts = tmpdir();
    let (m_cfg, m_flag) = (scripts.join("cfg-verify"), scripts.join("flag-verify"));
    let (d, home) = project(&format!(
        "no_auto_mcp = true\n[mission]\nverify = \"touch {}\"\n",
        m_cfg.display()
    ));
    let o = finish(with_provider(
        base_cmd(&d, &home, &["mission", "goal"]),
        &url,
    ));
    assert!(
        o.status.success(),
        "{} {}",
        text(&o.stdout),
        text(&o.stderr)
    );
    assert!(m_cfg.exists(), "config verify command never ran");
    std::fs::remove_file(&m_cfg).unwrap();
    let flag_verify = format!("touch {}", m_flag.display());
    let o = finish(with_provider(
        base_cmd(&d, &home, &["mission", "goal", "--verify", &flag_verify]),
        &url,
    ));
    assert!(
        o.status.success(),
        "{} {}",
        text(&o.stdout),
        text(&o.stderr)
    );
    assert!(m_flag.exists(), "flag verify must run");
    assert!(
        !m_cfg.exists(),
        "flag --verify must override the config verify"
    );
}

#[test]
fn mission_verify_timeout_from_config_is_used() {
    let (url, seen) = spawn_mock(vec![Step::Final("done\nMISSION_COMPLETE")]);
    let (d, home) =
        project("no_auto_mcp = true\n[mission]\nverify = \"sleep 30\"\nverify_timeout = 1\n");
    let started = Instant::now();
    let o = finish(with_provider(
        base_cmd(&d, &home, &["mission", "goal"]),
        &url,
    ));
    assert!(
        !o.status.success(),
        "a timed-out verify must fail the mission"
    );
    assert!(
        started.elapsed() < Duration::from_secs(25),
        "the 1s config timeout was not applied"
    );
    let all = serde_json::to_string(&*seen.lock().unwrap()).unwrap();
    assert!(all.contains("timed out after 1s"), "{all}");
}

// ------------------------------------------------------- face refusals

const NO_CONFIG_HINT: &str = "use --no-config to ignore the project config";

/// The refused command must exit non-zero, print the flag's message plus
/// the hint, and never have started (no stdout, no provider contact).
fn assert_refused_with_hint(config: &str, args: &[&str], needle: &str) {
    let (d, home) = project(config);
    let o = finish(base_cmd(&d, &home, args));
    let err = text(&o.stderr);
    assert!(!o.status.success(), "{args:?} must fail: {err}");
    assert!(err.contains(needle), "{args:?}: want {needle:?} in {err}");
    assert!(err.contains(NO_CONFIG_HINT), "{args:?}: no hint in {err}");
    assert_eq!(
        text(&o.stdout),
        "",
        "{args:?}: the command must not have run"
    );
    assert!(!err.contains("missing provider config"), "{err}");
    // `--no-config` drops the keys: the same command line no longer trips
    // the refusal (it fails later, if at all, for unrelated reasons).
    // (`serve` would start listening, so its control is skipped.)
    if args[0] == "serve" {
        return;
    }
    let mut with_flag = vec!["--no-config"];
    with_flag.extend_from_slice(args);
    let o = finish(base_cmd(&d, &home, &with_flag));
    assert!(
        !text(&o.stderr).contains(needle),
        "--no-config: {}",
        text(&o.stderr)
    );
}

#[cfg(feature = "shell-tools")]
#[test]
fn serve_refuses_config_hooks_like_the_flag() {
    let serve = ["serve", "--port", "0", "--token", "t"];
    assert_refused_with_hint(
        "on_pretool = [\"x\"]\n",
        &serve,
        "--on-pretool/--on-posttool are not supported by `tole serve`",
    );
    assert_refused_with_hint(
        "on_posttool = [\"x\"]\n",
        &serve,
        "--on-pretool/--on-posttool are not supported by `tole serve`",
    );
    assert_refused_with_hint(
        "skill = [\"s/SKILL.md\"]\n",
        &serve,
        "--skill is not supported by `tole serve`",
    );
    assert_refused_with_hint(
        "no_skills = true\n",
        &serve,
        "--no-skills is not supported by `tole serve`",
    );
    #[cfg(feature = "mcp")]
    assert_refused_with_hint(
        "mcp_server = [\"a=b\"]\n",
        &serve,
        "--mcp-server is not supported by `tole serve`",
    );
}

#[cfg(feature = "shell-tools")]
#[test]
fn acp_refuses_config_hooks_skills_and_mcp_servers_like_the_flag() {
    assert_refused_with_hint(
        "on_pretool = [\"x\"]\n",
        &["acp"],
        "--on-pretool/--on-posttool are not supported by `tole acp`",
    );
    assert_refused_with_hint(
        "skill = [\"s\"]\n",
        &["acp"],
        "--skill is not supported by `tole acp`",
    );
    #[cfg(feature = "mcp")]
    assert_refused_with_hint(
        "mcp_server = [\"a=b\"]\n",
        &["acp"],
        "--mcp-server is not supported by `tole acp`",
    );
}

#[cfg(all(feature = "shell-tools", feature = "mcp"))]
#[test]
fn mcp_refuses_config_hooks_turnend_skills_and_servers_like_the_flag() {
    assert_refused_with_hint(
        "on_pretool = [\"x\"]\n",
        &["mcp"],
        "--on-pretool/--on-posttool are not supported by `tole mcp`",
    );
    assert_refused_with_hint(
        "on_turnend = [\"x\"]\n",
        &["mcp"],
        "--on-turnend is not supported by `tole mcp`",
    );
    assert_refused_with_hint(
        "no_skills = true\n",
        &["mcp"],
        "--no-skills is not supported by `tole mcp`",
    );
    #[cfg(feature = "mcp")]
    assert_refused_with_hint(
        "mcp_server = [\"a=b\"]\n",
        &["mcp"],
        "--mcp-server is not supported by `tole mcp`",
    );
}

#[test]
fn mission_refuses_config_plan_mode_hooks_skills_and_servers_like_the_flag() {
    let m = ["mission", "goal"];
    assert_refused_with_hint(
        "plan_mode = true\n",
        &m,
        "--plan-mode has no meaning for a mission",
    );
    #[cfg(feature = "shell-tools")]
    assert_refused_with_hint(
        "on_pretool = [\"x\"]\n",
        &m,
        "--on-pretool/--on-posttool are not supported by `tole run mission`",
    );
    #[cfg(feature = "shell-tools")]
    assert_refused_with_hint(
        "on_turnend = [\"x\"]\n",
        &m,
        "--on-turnend is not supported by `tole run mission`",
    );
    assert_refused_with_hint(
        "skill = [\"s\"]\n",
        &m,
        "--skill is not supported by `tole mission`",
    );
    #[cfg(feature = "mcp")]
    assert_refused_with_hint(
        "mcp_server = [\"a=b\"]\n",
        &m,
        "--mcp-server is not supported by `tole mission`",
    );
}

/// Parity: the very same refusal from a FLAG carries no config hint.
#[cfg(feature = "shell-tools")]
#[test]
fn a_flag_sourced_refusal_has_no_config_hint() {
    let d = tmpdir();
    let o = finish(base_cmd(
        &d,
        &tmpdir(),
        &["--on-pretool", "x", "serve", "--port", "0", "--token", "t"],
    ));
    let err = text(&o.stderr);
    assert!(!o.status.success());
    assert!(err.contains("--on-pretool/--on-posttool are not supported by `tole serve`"));
    assert!(!err.contains("--no-config"), "{err}");
}

// ------------------------------------------------ --no-config / features

#[test]
fn no_config_drops_every_sensitive_key() {
    let (url, seen) = spawn_mock(vec![Step::Final("ok")]);
    let (d, home) = project(
        "plan_mode = true\nno_auto_mcp = true\ntrust = [\"internal\"]\n\
         allow = [\"write_*\"]\nskill = [\"nope/SKILL.md\"]\n",
    );
    // `skill` points at a missing file: applied, it would fail startup.
    let o = finish(with_provider(base_cmd(&d, &home, &["run", "hi"]), &url));
    assert!(!o.status.success(), "the config skill path must be applied");
    let o = finish(with_provider(
        base_cmd(&d, &home, &["--no-config", "run", "hi"]),
        &url,
    ));
    assert!(o.status.success(), "{}", text(&o.stderr));
    let body = seen.lock().unwrap()[0].clone();
    assert!(tool_names(&body).iter().any(|n| n == "write_file"));
    assert!(!system_text(&body).contains("PLAN MODE"));
}

#[cfg(not(feature = "mcp"))]
#[test]
fn mcp_keys_are_a_loud_error_in_a_build_without_the_mcp_feature() {
    for key in ["mcp_server = [\"a=b\"]\n", "no_auto_mcp = true\n"] {
        let (d, home) = project_raw(key);
        let o = finish(base_cmd(&d, &home, &["sessions"]));
        let err = text(&o.stderr);
        assert!(!o.status.success(), "{err}");
        assert!(err.contains("needs the `mcp` feature"), "{err}");
        assert!(err.contains(key.split(' ').next().unwrap()), "{err}");
    }
}

#[cfg(not(feature = "shell-tools"))]
#[test]
fn hook_and_memory_keys_are_a_loud_error_in_a_build_without_shell_tools() {
    for key in [
        "on_pretool = [\"x\"]\n",
        "on_posttool = [\"x\"]\n",
        "on_turnend = [\"x\"]\n",
        "memory = \"uteke\"\n",
    ] {
        let (d, home) = project_raw(key);
        let o = finish(base_cmd(&d, &home, &["sessions"]));
        let err = text(&o.stderr);
        assert!(!o.status.success(), "{err}");
        assert!(err.contains("needs the `shell-tools` feature"), "{err}");
    }
}

// ------------------------------------------------------------ config check

#[test]
fn config_check_reports_effective_values_for_the_sensitive_keys() {
    let (d, home) = project(
        "plan_mode = true\ntrust = [\"internal\"]\nallow = [\"write_*\"]\non_pretool = [\"p\"]\n\
         skill = [\"s/SKILL.md\"]\nno_skills = false\n[mission]\nverify = \"v\"\nverify_timeout = 7\n",
    );
    let o = finish(base_cmd(
        &d,
        &home,
        &["--plan-mode", "--trust", "none", "config", "check"],
    ));
    let out = text(&o.stdout);
    assert!(o.status.success(), "{out} {}", text(&o.stderr));
    assert!(!out.contains("not applied"), "{out}");
    assert!(
        out.contains("plan_mode = true  [config]  effective: true  (flag)"),
        "{out}"
    );
    assert!(out.contains("effective: [\"none\"]  (flag)"), "{out}");
    assert!(out.contains("effective: [\"write_*\"]  (config)"), "{out}");
    assert!(
        out.contains("effective: [\"s/SKILL.md\"]  (config)"),
        "{out}"
    );
    assert!(
        out.contains("no_skills = false  [config]  effective: false  (config)"),
        "{out}"
    );
    assert!(out.contains("flags can only turn this on"), "{out}");
    assert!(
        out.contains("mission.verify_timeout = 7  [config]  effective: 7  (config)"),
        "{out}"
    );
    // Env layer for trust is reported with its variable name.
    let o = finish(with_env(
        base_cmd(&d, &home, &["config", "check"]),
        "TOLE_TRUST",
        "read_only",
    ));
    let out = text(&o.stdout);
    assert!(
        out.contains("effective: [\"read_only\"]  (env TOLE_TRUST)"),
        "{out}"
    );
}
