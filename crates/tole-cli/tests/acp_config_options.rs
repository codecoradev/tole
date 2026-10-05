//! #176 ACP session config options — the model picker, end-to-end with
//! the REAL `tole acp` binary: advertisement on session/new, durable
//! override via session/set_config_option (register write, restored on
//! session/load in a fresh process), and the switched model actually
//! reaching the provider request body.

use serde_json::{json, Value};
use std::io::{BufRead, Read, Write};
use std::net::TcpListener;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::Duration;

// ---------------------------------------------------------------------------
// ACP process harness (same shape as acp_handshake.rs, plus env control)
// ---------------------------------------------------------------------------

struct AcpProcess {
    child: Child,
    rx: mpsc::Receiver<String>,
}

impl AcpProcess {
    /// Spawn `tole acp` with a hermetic env: the given vars set, the
    /// developer-shell tole vars stripped (a leaked TOLE_MEMORY would
    /// pull uteke recall into the prompt test).
    fn spawn_with(env: &[(&str, &str)]) -> Self {
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
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let reader = std::io::BufReader::new(stdout);
            for line in reader.lines().map_while(Result::ok) {
                if tx.send(line).is_err() {
                    break;
                }
            }
        });
        Self { child, rx }
    }

    fn send(&mut self, v: &Value) {
        writeln!(self.child.stdin.as_mut().expect("stdin"), "{v}").unwrap();
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

    fn initialize(&mut self, id: u64) {
        self.send(&json!({
            "jsonrpc": "2.0", "id": id, "method": "initialize",
            "params": {"protocolVersion": 1, "clientCapabilities": {}}
        }));
        let init = self.wait_response(id, Duration::from_secs(20));
        assert_eq!(init["result"]["protocolVersion"], 1);
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
        "tole-acp-cfg-{tag}-{}-{:x}",
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
// Mock provider: records the model field of every request, always finals
// ---------------------------------------------------------------------------

fn spawn_mock() -> (String, Arc<Mutex<Vec<String>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let seen: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let seen_inner = Arc::clone(&seen);
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let seen_conn = Arc::clone(&seen_inner);
            std::thread::spawn(move || {
                let mut s = stream;
                let mut buf = Vec::new();
                let mut chunk = [0u8; 16384];
                let mut body = Value::Null;
                let deadline = std::time::Instant::now() + Duration::from_secs(10);
                while std::time::Instant::now() < deadline {
                    let n = s.read(&mut chunk).unwrap_or(0);
                    if n > 0 {
                        buf.extend_from_slice(&chunk[..n]);
                    }
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
                seen_conn
                    .lock()
                    .unwrap()
                    .push(body["model"].as_str().unwrap_or("<none>").to_string());
                let reply = json!({
                    "id": "chatcmpl-mock", "object": "chat.completion", "created": 1,
                    "model": body["model"].as_str().unwrap_or("mock"),
                    "choices": [{
                        "index": 0,
                        "message": {"role": "assistant", "content": "PICKER_OK"},
                        "finish_reason": "stop"
                    }],
                    "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2}
                });
                let data = reply.to_string();
                let resp = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    data.len(),
                    data
                );
                let _ = s.write_all(resp.as_bytes());
            });
        }
    });
    (format!("http://{addr}/v1"), seen)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// The model picker is advertised on session/new when TOLE_MODELS is set,
/// with the env default as currentValue.
#[test]
fn session_new_advertises_model_picker() {
    let mut acp = AcpProcess::spawn_with(&[
        ("TOLE_MODEL", "model-a"),
        ("TOLE_MODELS", "model-a, model-b"),
    ]);
    acp.initialize(1);
    let cwd = temp_cwd("advertise");
    acp.send(&json!({
        "jsonrpc": "2.0", "id": 2, "method": "session/new",
        "params": {"cwd": cwd.to_string_lossy()}
    }));
    let created = acp.wait_response(2, Duration::from_secs(20));
    let opts = created["result"]["configOptions"]
        .as_array()
        .expect("configOptions advertised");
    assert_eq!(opts.len(), 1);
    assert_eq!(opts[0]["id"], "model");
    assert_eq!(opts[0]["category"], "model");
    assert_eq!(opts[0]["type"], "select");
    assert_eq!(opts[0]["currentValue"], "model-a");
    assert_eq!(opts[0]["options"].as_array().expect("options").len(), 2);
}

/// Back-compat: without TOLE_MODELS nothing is advertised (the host's
/// picker stays hidden — exactly pre-#176 behavior).
#[test]
fn no_tole_models_advertises_nothing() {
    let mut acp = AcpProcess::spawn_with(&[("TOLE_MODEL", "model-a")]);
    acp.initialize(1);
    let cwd = temp_cwd("noplcker");
    acp.send(&json!({
        "jsonrpc": "2.0", "id": 2, "method": "session/new",
        "params": {"cwd": cwd.to_string_lossy()}
    }));
    let created = acp.wait_response(2, Duration::from_secs(20));
    assert!(
        created["result"].get("configOptions").is_none(),
        "no configOptions without TOLE_MODELS: {}",
        created["result"]
    );
}

/// set_config_option switches the picker, validates values, and the
/// override survives a session/load in a FRESH process (durable
/// register write, not process memory).
#[test]
fn set_config_option_switches_validates_and_persists() {
    let mut acp = AcpProcess::spawn_with(&[
        ("TOLE_MODEL", "model-a"),
        ("TOLE_MODELS", "model-a,model-b"),
    ]);
    acp.initialize(1);
    let cwd = temp_cwd("switch");
    acp.send(&json!({
        "jsonrpc": "2.0", "id": 2, "method": "session/new",
        "params": {"cwd": cwd.to_string_lossy()}
    }));
    let session_id = acp.wait_response(2, Duration::from_secs(20))["result"]["sessionId"]
        .as_str()
        .unwrap()
        .to_string();

    // Switch to model-b: the response carries the FULL config state.
    acp.send(&json!({
        "jsonrpc": "2.0", "id": 3, "method": "session/set_config_option",
        "params": {"sessionId": session_id, "configId": "model", "value": "model-b"}
    }));
    let switched = acp.wait_response(3, Duration::from_secs(20));
    assert_eq!(
        switched["result"]["configOptions"][0]["currentValue"], "model-b",
        "response must carry the complete config state: {switched}"
    );

    // Invalid value / unknown configId / unknown session: JSON-RPC errors.
    for (id, params, why) in [
        (
            10u64,
            json!({"sessionId": session_id, "configId": "model", "value": "model-z"}),
            "unknown model value",
        ),
        (
            11,
            json!({"sessionId": session_id, "configId": "vibe", "value": "model-b"}),
            "unknown configId",
        ),
        (
            12,
            json!({"sessionId": "acp-does-not-exist", "configId": "model", "value": "model-b"}),
            "unknown session",
        ),
    ] {
        acp.send(&json!({
            "jsonrpc": "2.0", "id": id, "method": "session/set_config_option",
            "params": params
        }));
        let err = acp.wait_response(id, Duration::from_secs(20));
        assert!(err.get("error").is_some(), "{why} must error: {err}");
    }

    // Fresh process, session/load: the override is still model-b.
    let mut acp2 = AcpProcess::spawn_with(&[
        ("TOLE_MODEL", "model-a"),
        ("TOLE_MODELS", "model-a,model-b"),
    ]);
    acp2.initialize(1);
    acp2.send(&json!({
        "jsonrpc": "2.0", "id": 2, "method": "session/load",
        "params": {"cwd": cwd.to_string_lossy(), "sessionId": session_id}
    }));
    let loaded = acp2.wait_response(2, Duration::from_secs(20));
    assert_eq!(loaded["result"]["sessionId"], session_id);
    assert_eq!(
        loaded["result"]["configOptions"][0]["currentValue"], "model-b",
        "durable override must survive a fresh process: {loaded}"
    );
}

/// The switched model actually reaches the provider request body —
/// per-session (a second session without an override still uses the env
/// default), applied from the durable register on every turn.
#[test]
fn prompt_uses_switched_model_per_session() {
    let (base_url, seen) = spawn_mock();
    let mut acp = AcpProcess::spawn_with(&[
        ("TOLE_BASE_URL", base_url.as_str()),
        ("TOLE_MODEL", "model-a"),
        ("TOLE_MODELS", "model-a,model-b"),
        ("TOLE_API_KEY", "sk-test"),
    ]);
    acp.initialize(1);
    let cwd = temp_cwd("prompt");
    acp.send(&json!({
        "jsonrpc": "2.0", "id": 2, "method": "session/new",
        "params": {"cwd": cwd.to_string_lossy()}
    }));
    let session_id = acp.wait_response(2, Duration::from_secs(20))["result"]["sessionId"]
        .as_str()
        .unwrap()
        .to_string();
    acp.send(&json!({
        "jsonrpc": "2.0", "id": 3, "method": "session/set_config_option",
        "params": {"sessionId": session_id, "configId": "model", "value": "model-b"}
    }));
    let switched = acp.wait_response(3, Duration::from_secs(20));
    assert_eq!(
        switched["result"]["configOptions"][0]["currentValue"],
        "model-b"
    );

    acp.send(&json!({
        "jsonrpc": "2.0", "id": 4, "method": "session/prompt",
        "params": {"sessionId": session_id, "prompt": [{"type": "text", "text": "hi"}]}
    }));
    let done = acp.wait_response(4, Duration::from_secs(30));
    assert_eq!(done["result"]["stopReason"], "end_turn", "{done}");
    {
        let seen = seen.lock().unwrap();
        assert!(
            seen.iter().all(|m| m == "model-b"),
            "every provider request must carry the override: {seen:?}"
        );
    }

    // A SECOND session with no override keeps the env default — the
    // override is per-session, not process-global.
    acp.send(&json!({
        "jsonrpc": "2.0", "id": 5, "method": "session/new",
        "params": {"cwd": cwd.to_string_lossy()}
    }));
    let session2 = acp.wait_response(5, Duration::from_secs(20))["result"]["sessionId"]
        .as_str()
        .unwrap()
        .to_string();
    acp.send(&json!({
        "jsonrpc": "2.0", "id": 6, "method": "session/prompt",
        "params": {"sessionId": session2, "prompt": [{"type": "text", "text": "hi"}]}
    }));
    let done2 = acp.wait_response(6, Duration::from_secs(30));
    assert_eq!(done2["result"]["stopReason"], "end_turn", "{done2}");
    {
        let seen = seen.lock().unwrap();
        assert_eq!(seen.last().expect("second prompt"), "model-a");
    }
}
