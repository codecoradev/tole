//! `tole cron` agent mode (#579 part 2) against a content-driven mock
//! provider: one durable session per run, unattended approvals (refused
//! standalone, parked in `/approvals` under serve), budgets, model pin.
#![cfg(feature = "shell-tools")]

use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};

use serde_json::{json, Value};
use tole_cli::approvals::ApprovalQueue;
use tole_cli::cron::{self, store, CreateSpec, TickOptions};
use tole_core::ledger::{DeliveryStatus, RunStatus};

/// 2026-01-01T00:00:00Z.
const T: i64 = 1_767_225_600;

/// Models the mock was asked for, in order.
fn models() -> &'static Mutex<Vec<String>> {
    static M: OnceLock<Mutex<Vec<String>>> = OnceLock::new();
    M.get_or_init(|| Mutex::new(Vec::new()))
}

fn completion(message: Value) -> Value {
    json!({"id": "c", "object": "chat.completion", "created": 1, "model": "mock",
        "choices": [{"index": 0, "message": message,
            "finish_reason": if message["tool_calls"].is_array() { "tool_calls" } else { "stop" }}],
        "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2}})
}

fn tool_call(name: &str, args: Value) -> Value {
    json!({"role": "assistant", "content": null, "tool_calls": [{"id": "call_1",
        "type": "function", "function": {"name": name, "arguments": args.to_string()}}]})
}

/// The reply for one request: keyed on the first user message's text.
fn reply(body: &Value) -> Value {
    let msgs = body["messages"].as_array().cloned().unwrap_or_default();
    let prompt = msgs
        .iter()
        .find(|m| m["role"] == "user")
        .and_then(|m| m["content"].as_str())
        .unwrap_or_default()
        .to_string();
    let after_tool = msgs.last().is_some_and(|m| m["role"] == "tool");
    if prompt.contains("WRITE") && !after_tool {
        completion(tool_call(
            "write_file",
            json!({"path": "out.txt", "content": "written by cron"}),
        ))
    } else if prompt.contains("LOOP") {
        completion(tool_call("read_file", json!({"path": "missing.txt"})))
    } else if after_tool {
        completion(json!({"role": "assistant", "content": "finished after the tool"}))
    } else {
        completion(json!({"role": "assistant", "content": "hello from the mock"}))
    }
}

fn http(stream: bool, reply: &Value) -> String {
    if !stream {
        let data = reply.to_string();
        return format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{data}",
            data.len()
        );
    }
    let msg = &reply["choices"][0]["message"];
    let mut out = String::new();
    if let Some(c) = msg["content"].as_str() {
        out.push_str(&format!(
            "data: {}\n\n",
            json!({"choices": [{"index": 0, "delta": {"content": c}, "finish_reason": null}]})
        ));
    }
    if let Some(tcs) = msg["tool_calls"].as_array() {
        for (i, tc) in tcs.iter().enumerate() {
            out.push_str(&format!(
                "data: {}\n\n",
                json!({"choices": [{"index": 0, "delta": {"tool_calls": [{"index": i,
                    "id": tc["id"], "function": {"name": tc["function"]["name"],
                    "arguments": tc["function"]["arguments"]}}]}, "finish_reason": null}]})
            ));
        }
    }
    out.push_str(&format!(
        "data: {}\n\ndata: [DONE]\n\n",
        json!({"choices": [], "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2}})
    ));
    format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{out}",
        out.len()
    )
}

/// Starts the mock once and points the provider env at it.
fn provider() {
    static ONCE: OnceLock<()> = OnceLock::new();
    ONCE.get_or_init(|| {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                std::thread::spawn(move || {
                    let mut s = stream;
                    let (mut buf, mut chunk) = (Vec::new(), [0u8; 16384]);
                    let mut body = Value::Null;
                    loop {
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
                    if let Some(m) = body["model"].as_str() {
                        models().lock().unwrap().push(m.to_string());
                    }
                    let stream = body["stream"].as_bool() == Some(true);
                    let _ = s.write_all(http(stream, &reply(&body)).as_bytes());
                });
            }
        });
        std::env::set_var("TOLE_BASE_URL", format!("http://{addr}/v1"));
        std::env::set_var("TOLE_MODEL", "mock-model");
        std::env::set_var("TOLE_API_KEY", "sk-test-cron-agent");
    });
}

fn tmp(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("tole-cron-agent-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d.canonicalize().unwrap()
}

fn agent(dir: &Path, id: &str, prompt: &str) -> CreateSpec {
    CreateSpec {
        id: Some(id.into()),
        schedule: "* * * * *".into(),
        prompt: Some(prompt.into()),
        workdir: dir.to_path_buf(),
        ..CreateSpec::default()
    }
}

/// Runs one due occurrence and returns `(status, session_id)`.
fn run_once(dir: &Path, opts: &TickOptions) -> (RunStatus, Option<String>, String) {
    let r = cron::tick(dir, T + 60, opts).unwrap();
    assert_eq!(r.ran.len(), 1, "{r:?}");
    let l = store::open_read(dir).unwrap();
    let done = l.state().done(&r.ran[0].0).unwrap().clone();
    (done.status, done.session_id, r.ran[0].0.clone())
}

#[test]
fn an_agent_run_is_a_durable_session_and_delivers_its_answer() {
    provider();
    let dir = tmp("ok");
    cron::create(&dir, agent(&dir, "hi", "say HELLO"), T).unwrap();
    let (status, session, fire) = run_once(&dir, &TickOptions::standalone(2));
    assert_eq!(status, RunStatus::Ok);
    let session = session.expect("the run links its session");
    assert!(dir
        .join(format!(".tole/sessions/{session}.jsonl"))
        .is_file());
    let l = store::open_read(&dir).unwrap();
    let d = l.state().deliveries(&fire).next().unwrap();
    assert_eq!(
        (d.target.as_str(), d.status),
        ("session", DeliveryStatus::Sent)
    );
    let out = std::fs::read_to_string(cron::tick::output_path(&dir, &fire)).unwrap();
    assert_eq!(out, "hello from the mock");
}

#[test]
fn standalone_tick_refuses_unlisted_writes_and_settles_awaiting_approval() {
    provider();
    let dir = tmp("refuse");
    cron::create(&dir, agent(&dir, "w", "please WRITE a file"), T).unwrap();
    let (status, _, _) = run_once(&dir, &TickOptions::standalone(2));
    assert_eq!(status, RunStatus::AwaitingApproval);
    assert!(
        !dir.join("out.txt").exists(),
        "a refused Write must not happen"
    );
}

#[test]
fn allowlisted_writes_run_unattended() {
    provider();
    let dir = tmp("allow");
    let mut spec = agent(&dir, "w", "please WRITE a file");
    spec.allow = vec!["write_file".into()];
    cron::create(&dir, spec, T).unwrap();
    let (status, _, _) = run_once(&dir, &TickOptions::standalone(2));
    assert_eq!(status, RunStatus::Ok);
    assert_eq!(
        std::fs::read_to_string(dir.join("out.txt")).unwrap(),
        "written by cron"
    );
}

#[test]
fn serve_hosted_runs_park_unlisted_writes_in_the_approvals_queue() {
    provider();
    let dir = tmp("park");
    cron::create(&dir, agent(&dir, "w", "please WRITE a file"), T).unwrap();
    let queue = ApprovalQueue::shared();
    let opts = TickOptions {
        approvals: Some(Arc::clone(&queue)),
        ..TickOptions::standalone(2)
    };
    let (status, session, _) = run_once(&dir, &opts);
    assert_eq!(status, RunStatus::AwaitingApproval);
    let pending = queue.list(session.as_deref());
    assert_eq!(pending.len(), 1, "parked, not auto-approved");
    assert_eq!(pending[0].tool, "write_file");
    assert!(!dir.join("out.txt").exists());
}

#[test]
fn the_step_budget_settles_budget() {
    provider();
    let dir = tmp("budget");
    let mut spec = agent(&dir, "l", "LOOP forever");
    spec.max_steps = Some(1);
    cron::create(&dir, spec, T).unwrap();
    let (status, _, fire) = run_once(&dir, &TickOptions::standalone(2));
    assert_eq!(status, RunStatus::Budget);
    let l = store::open_read(&dir).unwrap();
    let usage = l.state().done(&fire).unwrap().usage.clone().unwrap();
    assert_eq!(
        usage["error"], "step budget exceeded",
        "the job's own budget, not the turn cap"
    );
}

#[test]
fn the_model_pin_is_sent_to_the_provider() {
    provider();
    let dir = tmp("model");
    let mut spec = agent(&dir, "m", "say HELLO pinned");
    spec.model = Some("pinned-model-xyz".into());
    cron::create(&dir, spec, T).unwrap();
    let (status, _, _) = run_once(&dir, &TickOptions::standalone(2));
    assert_eq!(status, RunStatus::Ok);
    assert!(models()
        .lock()
        .unwrap()
        .iter()
        .any(|m| m == "pinned-model-xyz"));
}

fn usage_of(dir: &Path, fire: &str) -> Value {
    let l = store::open_read(dir).unwrap();
    l.state().done(fire).unwrap().usage.clone().unwrap()
}

#[test]
fn the_token_budget_settles_budget() {
    provider();
    let dir = tmp("tokens");
    let mut spec = agent(&dir, "t", "LOOP forever");
    spec.max_tokens = Some(1);
    cron::create(&dir, spec, T).unwrap();
    let (status, _, fire) = run_once(&dir, &TickOptions::standalone(2));
    assert_eq!(status, RunStatus::Budget);
    let usage = usage_of(&dir, &fire);
    assert_eq!(usage["error"], "token budget exceeded");
    assert!(usage["tokens"].as_u64().unwrap() >= 1, "{usage}");
}

#[test]
fn the_daily_token_cap_limits_then_stops_agent_runs_until_the_next_day() {
    provider();
    let dir = tmp("cap");
    cron::create(&dir, agent(&dir, "d", "say HELLO capped"), T).unwrap();
    let opts = TickOptions {
        daily_token_cap: Some(3),
        ..TickOptions::standalone(2)
    };
    let tick = |at: i64| {
        let r = cron::tick(&dir, at, &opts).unwrap();
        assert_eq!(r.ran.len(), 1, "{r:?}");
        let (fire, status) = r.ran[0].clone();
        (status, usage_of(&dir, &fire))
    };
    let (s1, u1) = tick(T + 60);
    assert_eq!(
        (s1, u1["tokens"].as_u64()),
        (RunStatus::Ok, Some(2)),
        "{u1}"
    );
    let (_, u2) = tick(T + 120);
    assert_eq!(u2["max_tokens"], 1, "only what is left of the cap: {u2}");
    let (s3, u3) = tick(T + 180);
    assert_eq!(s3, RunStatus::Budget);
    assert_eq!(
        (u3["tokens"].as_u64(), u3["error"].as_str()),
        (Some(0), Some("daily token cap reached"))
    );
    // A new UTC day starts from zero.
    let (s4, _) = tick(T + 86_400 + 60);
    assert_eq!(s4, RunStatus::Ok);
}

#[test]
fn a_cancelled_agent_run_settles_cancelled() {
    provider();
    let dir = tmp("cancel");
    let job = cron::create(&dir, agent(&dir, "c", "LOOP forever"), T).unwrap();
    let cancel = tole_core::cancel::CancelToken::new();
    cancel.cancel();
    let run = cron::agent::run_agent(&job, None, &cancel, 1_000);
    assert_eq!(run.status, RunStatus::Cancelled, "{run:?}");
}

#[test]
fn run_now_runs_an_agent_job_in_this_process() {
    provider();
    let dir = tmp("runnow");
    cron::create(&dir, agent(&dir, "n", "say HELLO now"), T).unwrap();
    let r = cron::run_now(&dir, "n", T + 30, &TickOptions::standalone(1)).unwrap();
    assert_eq!(r.ran.len(), 1);
    assert_eq!(r.ran[0].1, RunStatus::Ok);
}

#[test]
fn a_huge_minute_budget_saturates_and_is_capped_on_save() {
    assert_eq!(
        cron::agent::wall_budget(u64::MAX),
        std::time::Duration::from_secs(u64::MAX)
    );
    let dir = tmp("minutes");
    let mut spec = agent(&dir, "m", "hi");
    spec.max_minutes = Some(cron::agent::MAX_MINUTES + 1);
    assert!(cron::create(&dir, spec, T).is_err());
}
