//! `tole discuss` end to end (#588, owner-only entry point): the real binary
//! starts a discussion from typed arguments against a mock provider, shows
//! the cost, lists and shows it, and refuses what the owner may not ask.

use serde_json::{json, Value};
use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::PathBuf;
use std::process::{Command, Output};
use std::time::{Duration, Instant};

const SECRET: &str = "discuss-cli-secret-0123456789";

fn tmp(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("tole-discuss-cli-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d.canonicalize().unwrap()
}

/// A provider that answers participants with text and meets the checker's
/// criteria, citing the first entry it was shown.
fn spawn_mock() -> String {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    std::thread::spawn(move || {
        for mut s in listener.incoming().flatten() {
            let mut buf = Vec::new();
            let mut chunk = [0u8; 65536];
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
            let msgs = body["messages"].as_array().cloned().unwrap_or_default();
            let text = |role: &str| {
                msgs.iter()
                    .find(|m| m["role"] == role)
                    .and_then(|m| m["content"].as_str())
                    .unwrap_or("")
                    .to_string()
            };
            let (system, user) = (text("system"), text("user"));
            let reply = if system.contains("against its acceptance criteria") {
                let e = user
                    .lines()
                    .find_map(|l| l.strip_prefix("entry "))
                    .and_then(|l| l.split_whitespace().next())
                    .unwrap_or("e1")
                    .to_string();
                json!([{"met": true, "evidence": [e]}]).to_string()
            } else {
                format!("use sqlite {SECRET}")
            };
            let data = json!({
                "id": "c", "object": "chat.completion", "created": 1, "model": "mock",
                "choices": [{"index": 0, "finish_reason": "stop",
                    "message": {"role": "assistant", "content": reply}}],
                "usage": {"prompt_tokens": 120, "completion_tokens": 30, "total_tokens": 150}
            })
            .to_string();
            let _ = s.write_all(
                format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{data}",
                    data.len()
                )
                .as_bytes(),
            );
        }
    });
    format!("http://{addr}/v1")
}

fn run(home: &PathBuf, base: Option<&str>, args: &[&str]) -> Output {
    let mut c = Command::new(env!("CARGO_BIN_EXE_tole"));
    c.args(args)
        .env("CODECORA_HOME", home)
        .env("HOME", home)
        .env("TOLE_NO_UPDATE_CHECK", "1")
        .env("DISCUSS_CLI_TOKEN", SECRET)
        .env_remove("TOLE_BASE_URL")
        .env_remove("OPENAI_BASE_URL")
        .env_remove("TOLE_MODEL")
        .env_remove("OPENAI_MODEL");
    if let Some(b) = base {
        c.env("TOLE_BASE_URL", b)
            .env("TOLE_MODEL", "mock")
            .env("TOLE_API_KEY", "k");
    }
    c.output().unwrap()
}

fn out(o: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&o.stdout),
        String::from_utf8_lossy(&o.stderr)
    )
}

fn home_with_profiles(tag: &str) -> PathBuf {
    let home = tmp(tag);
    std::fs::create_dir_all(home.join("tole")).unwrap();
    std::fs::write(
        home.join("tole/config.toml"),
        "[profiles.coder]\n[profiles.ops]\n",
    )
    .unwrap();
    home
}

const START: [&str; 14] = [
    "discuss",
    "start",
    "--id",
    "pick-db",
    "--question",
    "Which database?",
    "--criterion",
    "a choice is named",
    "--participant",
    "coder",
    "--participant",
    "ops",
    "--budget",
    "100000",
];

#[test]
fn start_runs_shows_the_cost_and_is_listed_and_shown_redacted() {
    let home = home_with_profiles("e2e");
    let base = spawn_mock();
    let o = run(&home, Some(&base), &START);
    let text = out(&o);
    assert!(o.status.success(), "{text}");
    assert!(text.contains("Budget: 100000 tokens, a hard cap"), "{text}");
    assert!(text.contains("Tokens used: "), "{text}");
    assert!(!text.contains(SECRET), "{text}");

    let list = out(&run(&home, None, &["discuss", "list"]));
    assert!(list.contains("pick-db"), "{list}");
    let show = out(&run(&home, None, &["discuss", "show", "pick-db", "--json"]));
    assert!(show.contains("pick-db") && !show.contains(SECRET), "{show}");

    // Resuming a finished discussion prints its outcome without a provider.
    let again = run(&home, Some(&base), &["discuss", "resume", "pick-db"]);
    assert!(again.status.success(), "{}", out(&again));
    assert!(out(&again).contains("Outcome: "));
}

#[test]
fn dry_run_shows_the_cost_without_a_provider_and_writes_nothing() {
    let home = home_with_profiles("dry");
    let mut args = START.to_vec();
    args.push("--dry-run");
    let o = run(&home, None, &args);
    let text = out(&o);
    assert!(o.status.success(), "{text}");
    assert!(
        text.contains("up to 4 rounds, up to 16 model calls"),
        "{text}"
    );
    assert!(!home.join("tole/discussions").exists());
}

#[test]
fn a_participant_outside_the_config_and_a_missing_provider_are_refused() {
    let home = home_with_profiles("refuse");
    let mut args = START.to_vec();
    args[9] = "stranger";
    let o = run(&home, Some("http://127.0.0.1:1/v1"), &args);
    assert!(!o.status.success());
    assert!(out(&o).contains("unknown profile"), "{}", out(&o));

    let o = run(&home, None, &START);
    assert!(!o.status.success());
    assert!(out(&o).contains("missing provider config"), "{}", out(&o));
    assert!(!home.join("tole/discussions/pick-db.jsonl").exists());
}

#[test]
fn list_and_show_need_no_config_and_no_provider() {
    let home = tmp("empty");
    let o = run(&home, None, &["discuss", "list"]);
    assert!(o.status.success(), "{}", out(&o));
    assert!(out(&o).contains("no discussions"));
    let o = run(&home, None, &["discuss", "show", "nope"]);
    assert!(!o.status.success());
}
