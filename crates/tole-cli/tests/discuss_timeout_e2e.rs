//! The discussion request timeout end to end (#786): the real binary against
//! a provider that answers participants only after a delay. A timeout below
//! the delay fails the turn with `timeout`; a larger one lets it finish.

use serde_json::{json, Value};
use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::PathBuf;
use std::process::{Command, Output};
use std::time::{Duration, Instant};

fn tmp(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("tole-disc-timeout-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d.canonicalize().unwrap()
}

/// A provider whose participant turns take `delay` to answer (the checker is
/// instant); one thread per connection, so a client that gave up costs nothing.
fn spawn_slow_mock(delay: Duration) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    std::thread::spawn(move || {
        for mut s in listener.incoming().flatten() {
            std::thread::spawn(move || {
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
                    std::thread::sleep(delay);
                    "use sqlite".to_string()
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
            });
        }
    });
    format!("http://{addr}/v1")
}

fn home(tag: &str, user_config: &str) -> PathBuf {
    let home = tmp(tag);
    std::fs::create_dir_all(home.join("tole")).unwrap();
    std::fs::write(
        home.join("tole/config.toml"),
        format!("[profiles.coder]\n{user_config}"),
    )
    .unwrap();
    home
}

fn discuss(home: &PathBuf, base: &str, timeout_env: Option<&str>) -> Output {
    let mut c = Command::new(env!("CARGO_BIN_EXE_tole"));
    c.args([
        "discuss",
        "start",
        "--id",
        "slow",
        "--question",
        "Which database?",
        "--criterion",
        "a choice is named",
        "--participant",
        "coder",
        "--budget",
        "100000",
        "--max-rounds",
        "1",
    ])
    .env("CODECORA_HOME", home)
    .env("HOME", home)
    .env("TOLE_NO_UPDATE_CHECK", "1")
    .env("TOLE_BASE_URL", base)
    .env("TOLE_MODEL", "mock")
    .env("TOLE_API_KEY", "k")
    .env_remove("OPENAI_BASE_URL")
    .env_remove("OPENAI_MODEL")
    .env_remove("TOLE_DISCUSSION_TIMEOUT_SECS");
    if let Some(t) = timeout_env {
        c.env("TOLE_DISCUSSION_TIMEOUT_SECS", t);
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

#[test]
fn a_timeout_below_the_provider_delay_fails_the_turn_and_a_larger_one_succeeds() {
    let base = spawn_slow_mock(Duration::from_secs(3));

    let h = home("short", "");
    let o = discuss(&h, &base, Some("2"));
    let text = out(&o);
    assert!(!o.status.success(), "2 s timeout vs 3 s answer: {text}");
    assert!(text.contains("timeout"), "{text}");

    // The same mock with a 10 s timeout (here from the user config) succeeds.
    let h = home("long", "[discussions]\nrequest_timeout_secs = 10\n");
    let o = discuss(&h, &base, None);
    let text = out(&o);
    assert!(o.status.success(), "10 s timeout vs 3 s answer: {text}");
    assert!(text.contains("Outcome: "), "{text}");

    // The env beats the config: a 2 s env over a 10 s config fails again.
    let o = discuss(
        &home("env", "[discussions]\nrequest_timeout_secs = 10\n"),
        &base,
        Some("2"),
    );
    assert!(!o.status.success(), "{}", out(&o));
}

#[test]
fn an_invalid_timeout_env_is_a_startup_error_naming_the_variable() {
    let base = "http://127.0.0.1:1/v1";
    for bad in ["soon", "0", "99999"] {
        let o = discuss(&home("bad", ""), base, Some(bad));
        let text = out(&o);
        assert!(!o.status.success(), "{bad}: {text}");
        assert!(
            text.contains("TOLE_DISCUSSION_TIMEOUT_SECS"),
            "{bad}: {text}"
        );
    }
}
