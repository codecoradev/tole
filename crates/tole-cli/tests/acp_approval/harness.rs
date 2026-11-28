//! The `tole acp` process harness with a permission auto-responder.

use crate::common::{self, TestDir};
use serde_json::{json, Value};
use std::io::{BufRead, Write};
use std::process::{Child, ChildStdin, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::Duration;

#[derive(Clone, Copy, PartialEq)]
pub enum Policy {
    AllowOnce,
    AllowAlways,
}

pub struct AcpProcess {
    child: Child,
    /// Hermetic HOME (#993), removed after `Drop` has killed the child.
    _home: TestDir,
    rx: mpsc::Receiver<String>,
    stdin: Arc<Mutex<ChildStdin>>,
    /// How many session/request_permission requests arrived.
    pub permission_count: Arc<AtomicUsize>,
    /// The `kind` list offered by each permission request, in order.
    pub offered_kinds: Arc<Mutex<Vec<Vec<String>>>>,
    /// Every `session/update` notification, in arrival order (issue #196
    /// E2E: thought chunks + tool cards).
    pub updates: Arc<Mutex<Vec<Value>>>,
}

impl AcpProcess {
    pub fn spawn_with(env: &[(&str, &str)], policy: Policy) -> Self {
        let home = TestDir::new("tole-acp-home");
        let mut cmd = common::hermetic_command(env!("CARGO_BIN_EXE_tole"), home.path(), &[]);
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
            _home: home,
            rx,
            stdin,
            permission_count,
            offered_kinds,
            updates,
        }
    }

    pub fn send(&self, v: &Value) {
        let mut w = self.stdin.lock().unwrap();
        writeln!(w, "{v}").unwrap();
        w.flush().unwrap();
    }

    pub fn wait_response(&self, id: u64, timeout: Duration) -> Value {
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

    pub fn initialize(&self) {
        self.send(&json!({
            "jsonrpc": "2.0", "id": 1, "method": "initialize",
            "params": {"protocolVersion": 1, "clientCapabilities": {}}
        }));
        assert_eq!(
            self.wait_response(1, Duration::from_secs(20))["result"]["protocolVersion"],
            1
        );
    }

    pub fn new_session(&self, id: u64, cwd: &std::path::Path) -> Value {
        self.send(&json!({
            "jsonrpc": "2.0", "id": id, "method": "session/new",
            "params": {"cwd": cwd.to_string_lossy()}
        }));
        self.wait_response(id, Duration::from_secs(20))
    }

    pub fn prompt(&self, id: u64, session_id: &str, text: &str) -> String {
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

pub fn temp_cwd(tag: &str) -> TestDir {
    TestDir::new(&format!("tole-acp-appr-{tag}"))
}
