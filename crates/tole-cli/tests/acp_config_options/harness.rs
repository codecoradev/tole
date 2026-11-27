//! The `tole acp` process harness (same shape as acp_handshake.rs, plus env control).

use crate::common::{self, TestDir};
use serde_json::{json, Value};
use std::io::{BufRead, Write};
use std::process::{Child, Stdio};
use std::sync::mpsc;
use std::time::Duration;

pub struct AcpProcess {
    child: Child,
    /// Hermetic HOME (#993), removed after `Drop` has killed the child.
    _home: TestDir,
    rx: mpsc::Receiver<String>,
}

impl AcpProcess {
    /// Spawn `tole acp` with a hermetic env: the given vars set, the
    /// developer-shell tole vars stripped (a leaked TOLE_MEMORY would
    /// pull uteke recall into the prompt test).
    pub fn spawn_with(env: &[(&str, &str)]) -> Self {
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
            .env_remove("TOLE_MODELS")
            // OPENAI_* is read as a fallback provider config (#195 probe
            // uses it too) — a developer shell that sourced a real
            // gateway's OPENAI_* made the no-picker test see a live
            // /models answer. Same strip, fallback included.
            .env_remove("OPENAI_BASE_URL")
            .env_remove("OPENAI_API_KEY")
            .env_remove("OPENAI_MODEL");
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
        Self {
            child,
            _home: home,
            rx,
        }
    }

    pub fn send(&mut self, v: &Value) {
        writeln!(self.child.stdin.as_mut().expect("stdin"), "{v}").unwrap();
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

    pub fn initialize(&mut self, id: u64) {
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

pub fn temp_cwd(tag: &str) -> TestDir {
    TestDir::new(&format!("tole-acp-cfg-{tag}"))
}
