//! Golden pins for the system prompt a new session is created with, through
//! the real `tole` binary (#942, epic #941). Prompt and skill assembly is
//! being moved out of `main.rs`; none of these strings may change by
//! accident. A deliberate prompt change updates the literals here.
//!
//! Each case runs `tole run` once against a loopback mock provider, takes
//! the system message and the tool names of the FIRST request, and
//! normalizes the one dynamic part (the `Context:` line: cwd and date).

#![cfg(feature = "shell-tools")]

use std::io::{BufRead, Read, Write};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};

mod common;
use common::TestDir;

type Seen = Arc<Mutex<Vec<String>>>;

/// A provider that answers every request with a short final text (plain
/// or streamed, as asked) and records each request body.
fn mock_provider() -> (String, Seen) {
    let seen: Seen = Arc::new(Mutex::new(Vec::new()));
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}/v1", listener.local_addr().unwrap());
    let log = Arc::clone(&seen);
    std::thread::spawn(move || {
        for s in listener.incoming().flatten() {
            let mut r = std::io::BufReader::new(s.try_clone().unwrap());
            let mut len = 0usize;
            loop {
                let mut line = String::new();
                if r.read_line(&mut line).unwrap_or(0) == 0 || line.trim().is_empty() {
                    break;
                }
                if let Some(v) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                    len = v.trim().parse().unwrap_or(0);
                }
            }
            let mut body = vec![0; len];
            let _ = r.read_exact(&mut body);
            let body = String::from_utf8_lossy(&body).into_owned();
            let stream = serde_json::from_str::<serde_json::Value>(&body)
                .is_ok_and(|v| v["stream"].as_bool() == Some(true));
            log.lock().unwrap().push(body);
            let (kind, payload) = if stream {
                let delta =
                    r#"{"choices":[{"index":0,"delta":{"content":"done"},"finish_reason":null}]}"#;
                let usage = r#"{"choices":[],"usage":{"prompt_tokens":1,"completion_tokens":1,"total_tokens":2}}"#;
                (
                    "text/event-stream",
                    format!("data: {delta}\n\ndata: {usage}\n\ndata: [DONE]\n\n"),
                )
            } else {
                let c = r#"{"id":"c","object":"chat.completion","created":1,"model":"mock","choices":[{"index":0,"message":{"role":"assistant","content":"done"},"finish_reason":"stop"}],"usage":{"prompt_tokens":1,"completion_tokens":1,"total_tokens":2}}"#;
                ("application/json", c.to_string())
            };
            let mut s = s;
            let _ = write!(
                s,
                "HTTP/1.1 200 OK\r\nContent-Type: {kind}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{payload}",
                payload.len()
            );
        }
    });
    (url, seen)
}

struct Observed {
    system: String,
    tools: Vec<String>,
}

/// Run `tole run <args> "go"` in a fresh workdir and return what the
/// first provider request carried. `prep` may create files under the
/// temp dir and return extra args.
fn observe(tag: &str, env: &[(&str, &str)], prep: impl Fn(&TestDir) -> Vec<String>) -> Observed {
    let base = TestDir::new(&format!("tole-assemble-{tag}"));
    for d in ["home", "cron", "w"] {
        std::fs::create_dir_all(base.join(d)).unwrap();
    }
    let extra = prep(&base);
    let (url, seen) = mock_provider();
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_tole"));
    // `--no-auto-mcp`: a `cora` binary on PATH would otherwise attach the cora
    // MCP preset, which adds tools and (with both review tools) a review
    // section to the prompt, so the result would depend on the machine. The
    // flag and the preset exist only with the `mcp` feature (the `no-mcp`
    // CI profile has neither).
    #[cfg(feature = "mcp")]
    cmd.arg("--no-auto-mcp");
    cmd.arg("run")
        .args(&extra)
        .arg("go")
        .current_dir(base.join("w"))
        .env_remove("TOLE_SYSTEM_PROMPT")
        .env("CODECORA_HOME", base.join("home"))
        .env("TOLE_CRON_DIR", base.join("cron"))
        .env("TZDIR", "/nonexistent")
        .env("TOLE_BASE_URL", &url)
        .env("TOLE_MODEL", "mock-model")
        .env("TOLE_API_KEY", "sk-test-assemble")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for (k, v) in env {
        cmd.env(k, v);
    }
    let out = cmd.output().expect("spawn tole");
    assert!(
        out.status.success(),
        "tole run failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let bodies = seen.lock().unwrap();
    let first: serde_json::Value =
        serde_json::from_str(bodies.first().expect("no request")).unwrap();
    let msg = &first["messages"][0];
    assert_eq!(
        msg["role"], "system",
        "first message is not the system prompt"
    );
    let mut tools: Vec<String> = first["tools"]
        .as_array()
        .map(|a| {
            a.iter()
                .map(|t| t["function"]["name"].as_str().unwrap().to_string())
                .collect()
        })
        .unwrap_or_default();
    // Tools that exist only when an optional binary is on PATH (`uteke`,
    // `cora`) are not part of the pinned roster; the rest is.
    tools
        .retain(|t| !(t == "cora_search" || t.starts_with("uteke_") || t.starts_with("mcp_cora_")));
    tools.sort();
    Observed {
        system: normalize(msg["content"].as_str().expect("system content")),
        tools,
    }
}

/// Replace the cwd and the date of the `Context:` line with fixed tokens.
fn normalize(s: &str) -> String {
    let marker = "Context: working directory ";
    let Some(at) = s.find(marker) else {
        return s.to_string();
    };
    let rest = &s[at + marker.len()..];
    let (_cwd, tail) = rest.split_once("; today is ").expect("context line shape");
    let (date, after) = tail.split_once(" (UTC).").expect("context line end");
    assert_eq!(date.len(), 10, "date shape: {date}");
    format!("{}{marker}<CWD>; today is <DATE> (UTC).{after}", &s[..at])
}

const BASE: &str = "You are tole, a careful personal assistant. Tool discipline: for anything involving files, prefer the dedicated tools — read_file, write_file, edit_file — instead of run_command; they are safer and their approvals are what the user's --allow settings mean. Use run_command only for what those cannot do (pipes, builds, process control). Before running any package install (cargo add, bun add, npm install), verify the package name exists with the verify_package tool — hallucinated package names are a real supply-chain attack vector. Keep answers concise.";

const PLAN: &str = " PLAN MODE: only read-only tools exist in this session; explore and produce a plan — mutation tools are absent until the session is started without --plan-mode.";

const CONTEXT: &str = "\nContext: working directory <CWD>; today is <DATE> (UTC).";

const ALL_TOOLS: &[&str] = &[
    "agent_poll",
    "agent_start",
    "cron_create",
    "delegate",
    "delegate_poll",
    "delete_file",
    "discuss",
    "discuss_status",
    "discuss_stop",
    "edit_file",
    "gh",
    "git",
    "job_poll",
    "job_start",
    "read_file",
    "read_file_raw",
    "run_command",
    "skill_propose",
    "todo_read",
    "todo_write",
    "verify_package",
    "web_fetch",
    "web_search",
    "write_file",
];

const READ_ONLY_TOOLS: &[&str] = &[
    "job_poll",
    "read_file",
    "todo_read",
    "verify_package",
    "web_fetch",
    "web_search",
];

/// `web_search` exists only with the `web-builtin` feature (the `no-mcp`
/// CI profile builds without it); the pinned rosters list it, so drop it
/// from the expectation when the feature is off.
fn assert_case(o: &Observed, system: &str, tools: &[&str]) {
    assert_eq!(o.system, system);
    let want: Vec<&str> = tools
        .iter()
        .copied()
        .filter(|t| cfg!(feature = "web-builtin") || *t != "web_search")
        .collect();
    assert_eq!(o.tools, want);
}

/// A skill file in its own directory (the skill is named after the
/// directory); returns the `--skill <file>` args.
fn skill_args(b: &TestDir) -> Vec<String> {
    let dir = b.join("golden-skill");
    std::fs::create_dir_all(&dir).unwrap();
    let file = dir.join("SKILL.md");
    std::fs::write(
        &file,
        "---\nname: golden-skill\ndescription: A golden skill.\n---\nDo the golden thing.\n",
    )
    .unwrap();
    vec!["--skill".into(), file.display().to_string()]
}

#[test]
fn default_prompt_has_context_line_and_full_tool_roster() {
    let o = observe("default", &[], |_| vec![]);
    assert_case(&o, &format!("{BASE}{CONTEXT}"), ALL_TOOLS);
}

#[test]
fn plan_mode_appends_the_sentence_before_context_and_keeps_read_only_tools() {
    let o = observe("plan", &[], |_| vec!["--plan-mode".into()]);
    assert_case(&o, &format!("{BASE}{PLAN}{CONTEXT}"), READ_ONLY_TOOLS);
}

#[test]
fn env_override_replaces_the_default_prompt_with_no_context_line() {
    let o = observe("env", &[("TOLE_SYSTEM_PROMPT", "ENV PROMPT")], |_| vec![]);
    assert_case(&o, "ENV PROMPT", ALL_TOOLS);
}

#[test]
fn system_flag_wins_over_env_and_adds_no_context_line() {
    let o = observe("flag", &[], |_| {
        vec!["--system".into(), "FLAG PROMPT".into()]
    });
    assert_case(&o, "FLAG PROMPT", ALL_TOOLS);
    let o = observe("flag-env", &[("TOLE_SYSTEM_PROMPT", "ENV PROMPT")], |_| {
        vec!["--system".into(), "FLAG PROMPT".into()]
    });
    assert_case(&o, "FLAG PROMPT", ALL_TOOLS);
}

/// An explicit `--skill <file>` is the owner's own choice and is not
/// gated by the skill approval store (only DISCOVERED skills are, #591),
/// so no `TOLE_SKILL_APPROVAL` is needed; the isolated CODECORA_HOME and
/// the empty workdir discover nothing, so only the file's section shows.
#[test]
fn skill_file_section_follows_the_context_line() {
    let o = observe("skill", &[], skill_args);
    let want = format!("{BASE}{CONTEXT}\n\n# Skill: golden-skill\nDo the golden thing.\n");
    assert_case(&o, &want, ALL_TOOLS);
}

#[test]
fn skill_file_section_follows_an_overriding_prompt_too() {
    let o = observe("skill-sys", &[], |b| {
        let mut a = vec!["--system".to_string(), "FLAG PROMPT".to_string()];
        a.extend(skill_args(b));
        a
    });
    let want = "FLAG PROMPT\n\n# Skill: golden-skill\nDo the golden thing.\n";
    assert_case(&o, want, ALL_TOOLS);
}

#[test]
fn no_skills_leaves_the_skill_section_out() {
    let o = observe("noskills", &[], |b| {
        let mut a = vec!["--no-skills".to_string()];
        a.extend(skill_args(b));
        a
    });
    assert_case(&o, &format!("{BASE}{CONTEXT}"), ALL_TOOLS);
}
