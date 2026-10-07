//! #176 PR 2 — ACP approval controls, end-to-end with the REAL `tole
//! acp` binary: the `approval` config option (ask/auto), the
//! `allow_always` permission option (remembered per session), and the
//! Destructive never-remembered rule.

use serde_json::{json, Value};
use std::io::{BufRead, Read, Write};
use std::net::TcpListener;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};

/// Convert a plain chat-completion reply into an SSE stream body
/// (issue #196 phase 3 mocks): reasoning/content deltas, tool_call
/// fragments split across chunks (the concatenation contract), the
/// usage-bearing final chunk, and [DONE].
fn completion_to_sse(reply: &Value) -> String {
    let mut out = String::new();
    let msg = &reply["choices"][0]["message"];
    let model = reply["model"].as_str().unwrap_or("mock");
    let mut chunk_of = |delta: Value| {
        let c = json!({
            "id": "chatcmpl-sse", "object": "chat.completion.chunk", "created": 1,
            "model": model,
            "choices": [{"index": 0, "delta": delta, "finish_reason": null}],
            "usage": null
        });
        out.push_str(&format!("data: {c}\n\n"));
    };
    if let Some(r) = msg["reasoning"].as_str() {
        chunk_of(json!({"reasoning": r}));
    }
    if let Some(c) = msg["content"].as_str() {
        // Split content across two chunks to exercise accumulation and
        // the text_streamed dedup on the ACP side.
        if !c.is_empty() {
            let (a, b) = c.split_at(c.len().div_ceil(2));
            chunk_of(json!({"content": a}));
            chunk_of(json!({"content": b}));
        }
    }
    if let Some(tcs) = msg["tool_calls"].as_array() {
        for (i, tc) in tcs.iter().enumerate() {
            let name = tc["function"]["name"].as_str().unwrap_or("");
            let args = tc["function"]["arguments"].as_str().unwrap_or("{}");
            let (a1, a2) = args.split_at(args.len().div_ceil(2));
            chunk_of(json!({"tool_calls": [{"index": i, "id": tc["id"],
                "function": {"name": name, "arguments": a1}}]}));
            chunk_of(json!({"tool_calls": [{"index": i,
                "function": {"arguments": a2}}]}));
        }
    }
    if !msg["tool_calls"].is_null() {
        out.push_str(&format!(
            "data: {}\n\n",
            json!({"choices": [{"index": 0, "delta": {}, "finish_reason": "tool_calls"}], "usage": null})
        ));
    }
    out.push_str(&format!(
        "data: {}\n\n",
        json!({"choices": [], "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2}})
    ));
    out.push_str("data: [DONE]\n\n");
    out
}

fn sse_response(reply: &Value, streamed: bool) -> String {
    if streamed {
        let body = completion_to_sse(reply);
        format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            body.len(),
            body
        )
    } else {
        let data = reply.to_string();
        format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            data.len(),
            data
        )
    }
}

use std::time::Duration;

// ---------------------------------------------------------------------------
// ACP process harness with a permission auto-responder
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq)]
enum Policy {
    AllowOnce,
    AllowAlways,
}

struct AcpProcess {
    child: Child,
    rx: mpsc::Receiver<String>,
    stdin: Arc<Mutex<ChildStdin>>,
    /// How many session/request_permission requests arrived.
    permission_count: Arc<AtomicUsize>,
    /// The `kind` list offered by each permission request, in order.
    offered_kinds: Arc<Mutex<Vec<Vec<String>>>>,
    /// Every `session/update` notification, in arrival order (issue #196
    /// E2E: thought chunks + tool cards).
    updates: Arc<Mutex<Vec<Value>>>,
}

impl AcpProcess {
    fn spawn_with(env: &[(&str, &str)], policy: Policy) -> Self {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_tole"));
        cmd.arg("acp")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .env_remove("TOLE_MEMORY")
            .env_remove("TOLE_MEMORY_NAMESPACE")
            .env_remove("TOLE_SYSTEM_PROMPT")
            .env_remove("TOLE_TRUST")
            .env_remove("TOLE_AGENT_DEPTH")
            .env_remove("TOLE_BASE_URL")
            .env_remove("TOLE_MODEL")
            .env_remove("TOLE_API_KEY")
            .env_remove("TOLE_MODELS");
        for (k, v) in env {
            cmd.env(k, v);
        }
        let mut child = cmd.spawn().expect("spawn tole acp");
        let stdout = child.stdout.take().expect("stdout piped");
        let stdin = Arc::new(Mutex::new(child.stdin.take().expect("stdin piped")));
        let permission_count = Arc::new(AtomicUsize::new(0));
        let offered_kinds = Arc::new(Mutex::new(Vec::new()));
        let policy = Arc::new(Mutex::new(policy));
        let updates: Arc<Mutex<Vec<Value>>> = Arc::new(Mutex::new(Vec::new()));
        let updates_reader = Arc::clone(&updates);
        let (tx, rx) = mpsc::channel();
        let count = Arc::clone(&permission_count);
        let kinds = Arc::clone(&offered_kinds);
        let policy_reader = Arc::clone(&policy);
        let stdin_writer = Arc::clone(&stdin);
        std::thread::spawn(move || {
            let reader = std::io::BufReader::new(stdout);
            for line in reader.lines().map_while(Result::ok) {
                let Ok(msg) = serde_json::from_str::<Value>(&line) else {
                    continue;
                };
                // Answer permission requests per the policy; everything
                // else flows to the waiting main thread.
                if msg.get("method").and_then(Value::as_str) == Some("session/request_permission") {
                    let req_id = msg.get("id").and_then(Value::as_u64).unwrap_or(0);
                    count.fetch_add(1, Ordering::SeqCst);
                    kinds.lock().unwrap().push(
                        msg["params"]["options"]
                            .as_array()
                            .map(|os| {
                                os.iter()
                                    .filter_map(|o| o["kind"].as_str().map(str::to_string))
                                    .collect()
                            })
                            .unwrap_or_default(),
                    );
                    let option = match *policy_reader.lock().unwrap() {
                        Policy::AllowOnce => "allow-once",
                        Policy::AllowAlways => "allow-always",
                    };
                    let reply = json!({
                        "jsonrpc": "2.0", "id": req_id,
                        "result": {"outcome": {"outcome": "selected", "optionId": option}}
                    });
                    let mut w = stdin_writer.lock().unwrap();
                    let _ = writeln!(w, "{reply}");
                    let _ = w.flush();
                    continue;
                }
                if msg.get("method").and_then(Value::as_str) == Some("session/update") {
                    updates_reader
                        .lock()
                        .unwrap()
                        .push(msg["params"]["update"].clone());
                }
                if tx.send(line).is_err() {
                    break;
                }
            }
        });
        Self {
            child,
            rx,
            stdin,
            permission_count,
            offered_kinds,
            updates,
        }
    }

    fn send(&self, v: &Value) {
        let mut w = self.stdin.lock().unwrap();
        writeln!(w, "{v}").unwrap();
        w.flush().unwrap();
    }

    fn wait_response(&self, id: u64, timeout: Duration) -> Value {
        let deadline = std::time::Instant::now() + timeout;
        loop {
            let left = deadline.saturating_duration_since(std::time::Instant::now());
            assert!(left > Duration::ZERO, "timed out waiting for id {id}");
            match self.rx.recv_timeout(left) {
                Ok(line) => {
                    if let Ok(v) = serde_json::from_str::<Value>(&line) {
                        if v.get("id").and_then(Value::as_u64) == Some(id)
                            && (v.get("result").is_some() || v.get("error").is_some())
                        {
                            return v;
                        }
                    }
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    panic!("timed out waiting for id {id}");
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    panic!("acp process closed stdout");
                }
            }
        }
    }

    fn initialize(&self) {
        self.send(&json!({
            "jsonrpc": "2.0", "id": 1, "method": "initialize",
            "params": {"protocolVersion": 1, "clientCapabilities": {}}
        }));
        assert_eq!(
            self.wait_response(1, Duration::from_secs(20))["result"]["protocolVersion"],
            1
        );
    }

    fn new_session(&self, id: u64, cwd: &std::path::Path) -> Value {
        self.send(&json!({
            "jsonrpc": "2.0", "id": id, "method": "session/new",
            "params": {"cwd": cwd.to_string_lossy()}
        }));
        self.wait_response(id, Duration::from_secs(20))
    }

    fn prompt(&self, id: u64, session_id: &str, text: &str) -> String {
        self.send(&json!({
            "jsonrpc": "2.0", "id": id, "method": "session/prompt",
            "params": {"sessionId": session_id,
                       "prompt": [{"type": "text", "text": text}]}
        }));
        self.wait_response(id, Duration::from_secs(30))["result"]["stopReason"]
            .as_str()
            .unwrap()
            .to_string()
    }
}

impl Drop for AcpProcess {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn temp_cwd(tag: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "tole-acp-appr-{tag}-{}-{:x}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

// ---------------------------------------------------------------------------
// Mock provider: content-driven missions (last user message decides)
// ---------------------------------------------------------------------------

fn spawn_mock() -> String {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            std::thread::spawn(move || {
                let mut s = stream;
                let mut buf = Vec::new();
                let mut chunk = [0u8; 16384];
                let mut body = Value::Null;
                let mut streamed = false;
                let deadline = std::time::Instant::now() + Duration::from_secs(10);
                while std::time::Instant::now() < deadline {
                    let n = s.read(&mut chunk).unwrap_or(0);
                    if n > 0 {
                        buf.extend_from_slice(&chunk[..n]);
                    }
                    if let Some(h) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                        if let Ok(v) = serde_json::from_slice::<Value>(&buf[h + 4..]) {
                            streamed = v.get("stream").and_then(Value::as_bool) == Some(true);
                            body = v;
                            break;
                        }
                    }
                    if n == 0 {
                        break;
                    }
                }
                let msgs = body["messages"].as_array().cloned().unwrap_or_default();
                let last_user = msgs
                    .iter()
                    .rev()
                    .find(|m| m["role"] == "user")
                    .and_then(|m| m["content"].as_str())
                    .unwrap_or("")
                    .to_string();
                // Only a tool result AFTER the last user message counts:
                // an earlier turn's result must not make this turn final.
                let has_tool_result = match msgs.iter().rposition(|m| m["role"] == "user") {
                    Some(i) => msgs[i + 1..].iter().any(|m| m["role"] == "tool"),
                    None => false,
                };
                let reply = if !has_tool_result {
                    let (path, content) = if last_user.contains("DELETE_MISSION") {
                        ("victim.txt", "bye")
                    } else if last_user.contains("SECOND_WRITE") {
                        ("mission-2.txt", "two")
                    } else if last_user.contains("PLAN_MISSION") {
                        ("plan.txt", "planned")
                    } else {
                        ("mission-1.txt", "one")
                    };
                    let (name, args) = if last_user.contains("DELETE_MISSION") {
                        ("delete_file", json!({"path": path}))
                    } else if last_user.contains("PLAN_MISSION") {
                        (
                            "update_plan",
                            json!({"entries": [
                                {"content": "read the spec", "status": "completed"},
                                {"content": "write the plan", "status": "in_progress"},
                            ]}),
                        )
                    } else {
                        ("write_file", json!({"path": path, "content": content}))
                    };
                    json!({
                        "id": "chatcmpl-mock", "object": "chat.completion", "created": 1,
                        "model": "mock",
                        "choices": [{"index": 0, "message": {"role": "assistant",
                            "content": null,
                            "reasoning": "I will act on the mission, then report.",
                            "tool_calls": [{"id": "call_1", "type": "function",
                                "function": {"name": name, "arguments": args.to_string()}}]},
                            "finish_reason": "tool_calls"}],
                        "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2}
                    })
                } else {
                    json!({
                        "id": "chatcmpl-mock", "object": "chat.completion", "created": 1,
                        "model": "mock",
                        "choices": [{"index": 0, "message": {"role": "assistant",
                            "content": "MISSION_DONE"}, "finish_reason": "stop"}],
                        "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2}
                    })
                };
                let resp = sse_response(&reply, streamed);
                let _ = s.write_all(resp.as_bytes());
            });
        }
    });
    format!("http://{addr}/v1")
}

fn provider_env() -> (String, Vec<(String, String)>) {
    let base = spawn_mock();
    (
        base.clone(),
        vec![
            ("TOLE_BASE_URL".to_string(), base),
            ("TOLE_MODEL".to_string(), "mock-model".to_string()),
            ("TOLE_API_KEY".to_string(), "sk-test".to_string()),
        ],
    )
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// The approval selector is always advertised (ask default) and
/// switchable via session/set_config_option; bogus values error.
#[test]
fn approval_option_advertised_and_switchable() {
    let acp = AcpProcess::spawn_with(&[], Policy::AllowOnce);
    acp.initialize();
    let cwd = temp_cwd("switch");
    let created = acp.new_session(2, &cwd);
    let sid = created["result"]["sessionId"].as_str().unwrap().to_string();

    let opts = created["result"]["configOptions"]
        .as_array()
        .unwrap_or_else(|| panic!("no configOptions in {}", created["result"]));
    assert_eq!(opts[0]["id"], "approval");
    assert_eq!(opts[0]["category"], "mode");
    assert_eq!(opts[0]["currentValue"], "ask");

    acp.send(&json!({
        "jsonrpc": "2.0", "id": 3, "method": "session/set_config_option",
        "params": {"sessionId": sid, "configId": "approval", "value": "auto"}
    }));
    let switched = acp.wait_response(3, Duration::from_secs(20));
    assert_eq!(
        switched["result"]["configOptions"][0]["currentValue"],
        "auto"
    );

    acp.send(&json!({
        "jsonrpc": "2.0", "id": 4, "method": "session/set_config_option",
        "params": {"sessionId": sid, "configId": "approval", "value": "yolo"}
    }));
    assert!(acp
        .wait_response(4, Duration::from_secs(20))
        .get("error")
        .is_some());

    acp.send(&json!({
        "jsonrpc": "2.0", "id": 5, "method": "session/set_config_option",
        "params": {"sessionId": "acp-nope", "configId": "approval", "value": "auto"}
    }));
    assert!(acp
        .wait_response(5, Duration::from_secs(20))
        .get("error")
        .is_some());
}

/// approval=auto is the session-scoped `--yes`: the write executes with
/// ZERO permission requests, and the file really lands.
#[test]
fn auto_approval_runs_writes_without_permission() {
    let env_pairs = provider_env().1;
    let env: Vec<(&str, &str)> = env_pairs
        .iter()
        .map(|(k, v)| (k.as_str(), v.as_str()))
        .collect();
    let acp = AcpProcess::spawn_with(&env, Policy::AllowOnce);
    acp.initialize();
    let cwd = temp_cwd("auto");
    let sid = acp.new_session(2, &cwd)["result"]["sessionId"]
        .as_str()
        .unwrap()
        .to_string();
    acp.send(&json!({
        "jsonrpc": "2.0", "id": 3, "method": "session/set_config_option",
        "params": {"sessionId": sid, "configId": "approval", "value": "auto"}
    }));
    let _ = acp.wait_response(3, Duration::from_secs(20));

    assert_eq!(acp.prompt(4, &sid, "WRITE_MISSION please"), "end_turn");
    assert_eq!(
        acp.permission_count.load(Ordering::SeqCst),
        0,
        "auto approval must not surface permission requests"
    );
    assert!(cwd.join("mission-1.txt").exists(), "the write must execute");
}

/// allow_always is offered for Write calls and remembered within the
/// session: the SECOND write of the same tool prompts nothing more.
#[test]
fn allow_always_remembered_within_session() {
    let env_pairs = provider_env().1;
    let env: Vec<(&str, &str)> = env_pairs
        .iter()
        .map(|(k, v)| (k.as_str(), v.as_str()))
        .collect();
    let acp = AcpProcess::spawn_with(&env, Policy::AllowAlways);
    acp.initialize();
    let cwd = temp_cwd("always");
    let sid = acp.new_session(2, &cwd)["result"]["sessionId"]
        .as_str()
        .unwrap()
        .to_string();

    assert_eq!(acp.prompt(3, &sid, "WRITE_MISSION one"), "end_turn");
    assert_eq!(acp.permission_count.load(Ordering::SeqCst), 1);
    assert!(cwd.join("mission-1.txt").exists());
    // The offered options must include the allow_always kind.
    assert!(acp.offered_kinds.lock().unwrap()[0]
        .iter()
        .any(|k| k == "allow_always"));

    // Same tool again: remembered, no new permission request.
    assert_eq!(acp.prompt(4, &sid, "SECOND_WRITE two"), "end_turn");
    assert_eq!(
        acp.permission_count.load(Ordering::SeqCst),
        1,
        "allow_always must remember the tool for the session"
    );
    assert!(cwd.join("mission-2.txt").exists());
}

/// Destructive calls never offer allow_always and are never remembered
/// — even when the client echoes the option back (hostile/buggy
/// client): each call prompts again.
#[test]
fn destructive_never_remembered() {
    let env_pairs = provider_env().1;
    let env: Vec<(&str, &str)> = env_pairs
        .iter()
        .map(|(k, v)| (k.as_str(), v.as_str()))
        .collect();
    let acp = AcpProcess::spawn_with(&env, Policy::AllowAlways);
    acp.initialize();
    let cwd = temp_cwd("destr");
    std::fs::write(cwd.join("victim.txt"), "x").unwrap();
    let sid = acp.new_session(2, &cwd)["result"]["sessionId"]
        .as_str()
        .unwrap()
        .to_string();

    assert_eq!(acp.prompt(3, &sid, "DELETE_MISSION 1"), "end_turn");
    assert_eq!(acp.prompt(4, &sid, "DELETE_MISSION 2"), "end_turn");
    let count = acp.permission_count.load(Ordering::SeqCst);
    assert_eq!(count, 2, "each Destructive call must prompt again");
    for kinds in acp.offered_kinds.lock().unwrap().iter() {
        assert!(
            !kinds.iter().any(|k| k == "allow_always"),
            "Destructive must not offer allow_always: {kinds:?}"
        );
    }
}

/// Issue #196 phases 1+2, end-to-end: during a live turn the host
/// receives the model's reasoning as an agent_thought_chunk BEFORE the
/// tool card, the tool card carries the spec kind (write_file → edit)
/// with in_progress status, and the card completes with an output
/// preview — instead of a silent "Working…".
#[test]
fn observer_streams_thought_and_tool_cards() {
    let env_pairs = provider_env().1;
    let env: Vec<(&str, &str)> = env_pairs
        .iter()
        .map(|(k, v)| (k.as_str(), v.as_str()))
        .collect();
    let acp = AcpProcess::spawn_with(&env, Policy::AllowOnce);
    acp.initialize();
    let cwd = temp_cwd("observer");
    let sid = acp.new_session(2, &cwd)["result"]["sessionId"]
        .as_str()
        .unwrap()
        .to_string();
    // Auto approval: zero permission noise, pure lifecycle observation.
    acp.send(&json!({
        "jsonrpc": "2.0", "id": 3, "method": "session/set_config_option",
        "params": {"sessionId": sid, "configId": "approval", "value": "auto"}
    }));
    let _ = acp.wait_response(3, Duration::from_secs(20));

    assert_eq!(acp.prompt(4, &sid, "WRITE_MISSION observe"), "end_turn");

    let updates = acp.updates.lock().unwrap().clone();
    let kinds: Vec<&str> = updates
        .iter()
        .filter_map(|u| u["sessionUpdate"].as_str())
        .collect();
    assert!(
        kinds.contains(&"agent_thought_chunk"),
        "reasoning must surface as a thought chunk: {kinds:?}"
    );
    let thought = updates
        .iter()
        .find(|u| u["sessionUpdate"] == "agent_thought_chunk")
        .expect("thought");
    assert_eq!(
        thought["content"]["text"], "I will act on the mission, then report.",
        "the provider's reasoning field reaches the host verbatim"
    );
    let card = updates
        .iter()
        .find(|u| u["sessionUpdate"] == "tool_call" && u["name"] == "write_file")
        .expect("tool card");
    assert_eq!(card["kind"], "edit", "write_file maps to the edit kind");
    assert_eq!(card["status"], "in_progress");
    assert_eq!(card["toolCallId"].as_str().unwrap(), "tool-1");
    let done = updates
        .iter()
        .find(|u| u["sessionUpdate"] == "tool_call_update")
        .expect("card close");
    assert_eq!(done["status"], "completed");
    assert_eq!(
        done["toolCallId"], card["toolCallId"],
        "the completion must correlate with the started card (cora MAJOR)"
    );
    assert!(
        done["content"][0]["content"]["text"]
            .as_str()
            .unwrap()
            .len()
            <= 201,
        "preview is bounded"
    );
    // Ordering: thought strictly before the tool card.
    let thought_idx = kinds
        .iter()
        .position(|k| *k == "agent_thought_chunk")
        .unwrap();
    let card_idx = kinds.iter().position(|k| *k == "tool_call").unwrap();
    assert!(thought_idx < card_idx, "thought first, action after");
}

/// Issue #196 phase 4, end-to-end: the model's update_plan call reaches
/// the host as a standard `plan` session/update (Termul PlanPanel
/// shape), with validated/normalized entries.
#[test]
fn plan_updates_reach_the_host() {
    let env_pairs = provider_env().1;
    let env: Vec<(&str, &str)> = env_pairs
        .iter()
        .map(|(k, v)| (k.as_str(), v.as_str()))
        .collect();
    let acp = AcpProcess::spawn_with(&env, Policy::AllowOnce);
    acp.initialize();
    let cwd = temp_cwd("plan");
    let sid = acp.new_session(2, &cwd)["result"]["sessionId"]
        .as_str()
        .unwrap()
        .to_string();
    acp.send(&json!({
        "jsonrpc": "2.0", "id": 3, "method": "session/set_config_option",
        "params": {"sessionId": sid, "configId": "approval", "value": "auto"}
    }));
    let _ = acp.wait_response(3, Duration::from_secs(20));

    assert_eq!(acp.prompt(4, &sid, "PLAN_MISSION plan it"), "end_turn");

    let updates = acp.updates.lock().unwrap().clone();
    let plan = updates
        .iter()
        .find(|u| u["sessionUpdate"] == "plan")
        .expect("a plan update must reach the host");
    let entries = plan["entries"].as_array().expect("entries array");
    assert_eq!(entries.len(), 2);
    assert_eq!(entries[0]["content"], "read the spec");
    assert_eq!(entries[0]["status"], "completed");
    assert_eq!(entries[1]["priority"], "medium", "spec default applied");
}
