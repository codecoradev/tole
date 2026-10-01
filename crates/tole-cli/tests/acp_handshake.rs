//! D2 (issue #95): ACP handshake + session lifecycle, CI-safe (no LLM).
//! Spawns the real `tole acp` binary and speaks line-delimited JSON-RPC
//! with a deadline-guarded reader.

use serde_json::{json, Value};
use std::io::{BufRead, Write};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::time::Duration;

struct AcpProcess {
    child: Child,
    rx: mpsc::Receiver<String>,
}

impl AcpProcess {
    fn spawn() -> Self {
        let mut child = Command::new(env!("CARGO_BIN_EXE_tole"))
            .arg("acp")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn tole acp");
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

    /// Wait for a line that parses as JSON-RPC with the given id.
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
}

impl Drop for AcpProcess {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn temp_cwd(tag: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("tole-acp-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

#[test]
fn acp_initialize_and_session_new() {
    let mut acp = AcpProcess::spawn();
    let cwd = temp_cwd("init");

    acp.send(&json!({
        "jsonrpc": "2.0", "id": 1, "method": "initialize",
        "params": {"protocolVersion": 1, "clientCapabilities": {}}
    }));
    let init = acp.wait_response(1, Duration::from_secs(20));
    assert_eq!(init["result"]["protocolVersion"], 1);
    assert_eq!(init["result"]["agentCapabilities"]["loadSession"], true);

    acp.send(&json!({
        "jsonrpc": "2.0", "id": 2, "method": "session/new",
        "params": {"cwd": cwd.to_string_lossy()}
    }));
    let created = acp.wait_response(2, Duration::from_secs(20));
    let session_id = created["result"]["sessionId"]
        .as_str()
        .expect("sessionId")
        .to_string();
    assert!(session_id.starts_with("acp-"));

    // The durable session file exists under the session cwd.
    assert!(cwd
        .join(".tole/sessions")
        .join(format!("{session_id}.jsonl"))
        .exists());

    // Unknown methods surface a JSON-RPC error, not a hang.
    acp.send(&json!({
        "jsonrpc": "2.0", "id": 3, "method": "session/frobnicate", "params": {}
    }));
    let err = acp.wait_response(3, Duration::from_secs(20));
    assert!(err.get("error").is_some());
}

#[test]
fn acp_load_returns_existing_session() {
    let mut acp = AcpProcess::spawn();
    let cwd = temp_cwd("load");

    acp.send(&json!({
        "jsonrpc": "2.0", "id": 1, "method": "initialize",
        "params": {"protocolVersion": 1, "clientCapabilities": {}}
    }));
    let _ = acp.wait_response(1, Duration::from_secs(20));

    acp.send(&json!({
        "jsonrpc": "2.0", "id": 2, "method": "session/new",
        "params": {"cwd": cwd.to_string_lossy()}
    }));
    let session_id = acp.wait_response(2, Duration::from_secs(20))["result"]["sessionId"]
        .as_str()
        .expect("sessionId")
        .to_string();

    // Load the SAME session id in a fresh process-backed state: the
    // durable JSONL is the source of truth (D2 acceptance).
    let mut acp2 = AcpProcess::spawn();
    acp2.send(&json!({
        "jsonrpc": "2.0", "id": 1, "method": "initialize",
        "params": {"protocolVersion": 1, "clientCapabilities": {}}
    }));
    let _ = acp2.wait_response(1, Duration::from_secs(20));
    acp2.send(&json!({
        "jsonrpc": "2.0", "id": 2, "method": "session/load",
        "params": {"cwd": cwd.to_string_lossy(), "sessionId": session_id}
    }));
    let loaded = acp2.wait_response(2, Duration::from_secs(20));
    assert_eq!(loaded["result"]["sessionId"], session_id);
}

/// The map must survive across prompts (CodeCora scan regression: a
/// fresh Arc per prompt dropped every session after the first turn).
#[test]
fn acp_session_survives_multiple_session_news() {
    let mut acp = AcpProcess::spawn();
    let cwd = temp_cwd("multi");

    acp.send(&json!({
        "jsonrpc": "2.0", "id": 1, "method": "initialize",
        "params": {"protocolVersion": 1, "clientCapabilities": {}}
    }));
    let _ = acp.wait_response(1, Duration::from_secs(20));

    for n in 2..=4 {
        acp.send(&json!({
            "jsonrpc": "2.0", "id": n, "method": "session/new",
            "params": {"cwd": cwd.to_string_lossy()}
        }));
        let r = acp.wait_response(n, Duration::from_secs(20));
        assert!(
            r["result"]["sessionId"].is_string(),
            "session/new {n} failed"
        );
    }
}

/// Path traversal via sessionId is refused at the protocol surface.
#[test]
fn acp_rejects_traversal_session_ids() {
    let mut acp = AcpProcess::spawn();
    let cwd = temp_cwd("traversal");

    acp.send(&json!({
        "jsonrpc": "2.0", "id": 1, "method": "initialize",
        "params": {"protocolVersion": 1, "clientCapabilities": {}}
    }));
    let _ = acp.wait_response(1, Duration::from_secs(20));

    acp.send(&json!({
        "jsonrpc": "2.0", "id": 2, "method": "session/load",
        "params": {"cwd": cwd.to_string_lossy(), "sessionId": "../../etc/passwd"}
    }));
    let r = acp.wait_response(2, Duration::from_secs(20));
    assert!(r.get("error").is_some(), "traversal id must be an error");
}
