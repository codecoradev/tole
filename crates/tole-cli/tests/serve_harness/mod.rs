//! Shared harness for the `tole serve` end-to-end tests (#524): a real
//! serve process over a temp workspace, a scripted provider mock, and a
//! small SSE client.

#![allow(dead_code)]

use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use crate::common;

pub struct Serve {
    pub child: Child,
    pub port: u16,
    pub dir: std::path::PathBuf,
    /// Remove `dir` on drop (only the process that created it).
    pub own_dir: bool,
}

impl Serve {
    pub fn spawn(base: &str) -> Self {
        let dir = std::env::temp_dir().join(format!(
            "tole-entries-{}-{:x}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        Self::spawn_in(base, dir, true)
    }

    /// A serve process over an existing workspace (a restart, or a second
    /// process contending for the same session files).
    pub fn spawn_in(base: &str, dir: std::path::PathBuf, own_dir: bool) -> Self {
        let (child, port) = common::spawn_listening(|port| {
            let mut cmd = Command::new(env!("CARGO_BIN_EXE_tole"));
            cmd.args(["serve", "--port", &port.to_string(), "--token", "t0ken"])
                .args(["--workspace", dir.to_str().unwrap()])
                .current_dir(&dir)
                .env("TOLE_BASE_URL", base)
                .env("TOLE_MODEL", "mock-model")
                .env("TOLE_API_KEY", "sk-test")
                .stdin(Stdio::null())
                .stdout(Stdio::null());
            cmd
        });
        Self {
            child,
            port,
            dir,
            own_dir,
        }
    }

    /// Stop the process (its writer locks are released by the OS).
    pub fn kill(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }

    pub fn http_with(
        &self,
        method: &str,
        path: &str,
        auth: bool,
        body: Option<&str>,
    ) -> (u16, Value) {
        let mut s = std::net::TcpStream::connect(("127.0.0.1", self.port)).unwrap();
        let auth = if auth {
            "Authorization: Bearer t0ken\r\n"
        } else {
            ""
        };
        let req = match body {
            Some(b) => format!(
                "{method} {path} HTTP/1.1\r\n{auth}Content-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{b}",
                b.len()
            ),
            None => format!("{method} {path} HTTP/1.1\r\n{auth}Connection: close\r\n\r\n"),
        };
        s.write_all(req.as_bytes()).unwrap();
        let mut reader = BufReader::new(s);
        let mut status = String::new();
        reader.read_line(&mut status).unwrap();
        let code = status
            .split_whitespace()
            .nth(1)
            .and_then(|c| c.parse().ok())
            .unwrap_or(0);
        let mut payload = String::new();
        loop {
            let mut line = String::new();
            if reader.read_line(&mut line).unwrap() == 0 {
                break;
            }
            if line == "\r\n" || line == "\n" {
                reader.read_to_string(&mut payload).unwrap();
                break;
            }
        }
        (
            code,
            serde_json::from_str(payload.trim()).unwrap_or(Value::Null),
        )
    }

    pub fn http(&self, method: &str, path: &str, body: Option<&str>) -> (u16, Value) {
        self.http_with(method, path, true, body)
    }

    pub fn session_file(&self, id: &str) -> std::path::PathBuf {
        self.dir.join(".tole/sessions").join(format!("{id}.jsonl"))
    }
}

impl Drop for Serve {
    fn drop(&mut self) {
        self.kill();
        if self.own_dir {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }
}

/// Provider mock: answers every call with a short final text; a request
/// mentioning "slow" is answered after `slow_ms`; a conversation with
/// "TOOL_MISSION" asks for a `write_file` (needs approval).
pub fn spawn_mock(slow_ms: u64) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
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
                let streamed = body.get("stream").and_then(Value::as_bool) == Some(true);
                if body.to_string().contains("slow") {
                    std::thread::sleep(Duration::from_millis(slow_ms));
                }
                let text = "ENTRIES-ANSWER";
                let msgs = body["messages"].as_array().cloned().unwrap_or_default();
                let last_user = msgs.iter().rposition(|m| m["role"] == "user");
                let has_tool_result = last_user
                    .map(|i| msgs[i + 1..].iter().any(|m| m["role"] == "tool"))
                    .unwrap_or(false);
                // The conversation mentions TOOL_MISSION and no tool result
                // follows the last user message (after a denial the last user
                // message is the denial note, so the call is repeated for the
                // approved replay, then answered once a result is in).
                let wants_tool = !has_tool_result && body.to_string().contains("TOOL_MISSION");
                let tool_args = "{\"path\":\"approved.txt\",\"content\":\"remote ok\"}";
                let resp = if streamed {
                    let mut out = String::new();
                    if wants_tool {
                        out.push_str(&format!(
                            "data: {}\n\n",
                            json!({"choices": [{"index": 0, "delta": {"tool_calls": [
                                {"index": 0, "id": "call_1", "function": {
                                    "name": "write_file", "arguments": tool_args}}]},
                                "finish_reason": null}]})
                        ));
                    } else {
                        out.push_str(&format!(
                            "data: {}\n\n",
                            json!({"choices": [{"index": 0, "delta": {"content": text},
                                "finish_reason": null}]})
                        ));
                    }
                    out.push_str(&format!(
                        "data: {}\n\ndata: [DONE]\n\n",
                        json!({"choices": [], "usage": {"prompt_tokens": 1,
                            "completion_tokens": 1, "total_tokens": 2}})
                    ));
                    format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{out}",
                        out.len()
                    )
                } else {
                    let message = if wants_tool {
                        json!({"role": "assistant", "content": null,
                            "tool_calls": [{"id": "call_1", "type": "function",
                                "function": {"name": "write_file", "arguments": tool_args}}]})
                    } else {
                        json!({"role": "assistant", "content": text})
                    };
                    let data = json!({
                        "id": "c", "object": "chat.completion", "created": 1, "model": "mock",
                        "choices": [{"index": 0, "message": message,
                            "finish_reason": if wants_tool { "tool_calls" } else { "stop" }}],
                        "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2}
                    })
                    .to_string();
                    format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{data}",
                        data.len()
                    )
                };
                let _ = s.write_all(resp.as_bytes());
            });
        }
    });
    format!("http://{addr}/v1")
}

pub fn new_session(serve: &Serve) -> String {
    let (code, body) = serve.http("POST", "/sessions", Some("{}"));
    assert_eq!(code, 200, "{body}");
    body["sessionId"].as_str().unwrap().to_string()
}

pub fn prompt(serve: &Serve, id: &str, text: &str) {
    let (code, body) = serve.http(
        "POST",
        &format!("/sessions/{id}/prompt"),
        Some(&json!({"text": text}).to_string()),
    );
    assert_eq!(code, 200, "{body}");
}

/// One parsed SSE frame.
#[derive(Debug, Clone)]
pub struct Frame {
    pub id: Option<u64>,
    pub event: String,
    pub data: Value,
}

/// A live `GET .../events` connection read by a background thread, so tests
/// can wait for frames with a timeout.
pub struct Sse {
    rx: std::sync::mpsc::Receiver<Frame>,
    sock: std::net::TcpStream,
}

impl Sse {
    /// Open a stream. A non-200 answer is returned as `Err((status, body))`.
    pub fn open(serve: &Serve, path: &str, headers: &str, auth: bool) -> Result<Sse, (u16, Value)> {
        let mut s = std::net::TcpStream::connect(("127.0.0.1", serve.port)).unwrap();
        let auth = if auth {
            "Authorization: Bearer t0ken\r\n"
        } else {
            ""
        };
        write!(
            s,
            "GET {path} HTTP/1.1\r\n{auth}{headers}Connection: close\r\n\r\n"
        )
        .unwrap();
        let mut reader = BufReader::new(s.try_clone().unwrap());
        let mut status = String::new();
        reader.read_line(&mut status).unwrap();
        let code: u16 = status
            .split_whitespace()
            .nth(1)
            .and_then(|c| c.parse().ok())
            .unwrap_or(0);
        loop {
            let mut line = String::new();
            if reader.read_line(&mut line).unwrap() == 0 || line == "\r\n" {
                break;
            }
        }
        if code != 200 {
            let mut body = String::new();
            let _ = reader.read_to_string(&mut body);
            return Err((
                code,
                serde_json::from_str(body.trim()).unwrap_or(Value::Null),
            ));
        }
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let (mut id, mut event, mut data): (Option<u64>, String, String) =
                (None, String::new(), String::new());
            loop {
                let mut line = String::new();
                if reader.read_line(&mut line).unwrap_or(0) == 0 {
                    return;
                }
                let line = line.trim_end_matches(['\r', '\n']);
                if line.is_empty() {
                    if !event.is_empty() {
                        let f = Frame {
                            id,
                            event: std::mem::take(&mut event),
                            data: serde_json::from_str(&data).unwrap_or(Value::Null),
                        };
                        if tx.send(f).is_err() {
                            return;
                        }
                    }
                    id = None;
                    data.clear();
                } else if let Some(v) = line.strip_prefix("id: ") {
                    id = v.parse().ok();
                } else if let Some(v) = line.strip_prefix("event: ") {
                    event = v.to_string();
                } else if let Some(v) = line.strip_prefix("data: ") {
                    data.push_str(v);
                } // lines starting with ':' are comments (heartbeat)
            }
        });
        Ok(Sse { rx, sock: s })
    }

    pub fn next(&self, within: Duration) -> Option<Frame> {
        self.rx.recv_timeout(within).ok()
    }

    /// Frames up to and including the first one `pred` accepts; panics on
    /// timeout, naming what arrived.
    pub fn until(&self, within: Duration, pred: impl Fn(&Frame) -> bool) -> Vec<Frame> {
        let deadline = Instant::now() + within;
        let mut got = Vec::new();
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            match self.rx.recv_timeout(left) {
                Ok(f) => {
                    let done = pred(&f);
                    got.push(f);
                    if done {
                        return got;
                    }
                }
                Err(_) => panic!("timed out waiting for a frame; got {got:#?}"),
            }
        }
    }

    pub fn close(&self) {
        let _ = self.sock.shutdown(std::net::Shutdown::Both);
    }
}
