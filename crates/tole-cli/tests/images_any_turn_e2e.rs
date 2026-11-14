//! #559: images on ANY turn, end to end with the REAL binary and a mock
//! provider that records every request body.
//!
//! - `tole chat`: `/image <path>` queues an image for the NEXT message, and
//!   that message — the second turn — records its own references;
//! - `tole acp`: `promptCapabilities.image` is advertised, an `image` content
//!   block is spilled and recorded, an image-only prompt runs, other non-text
//!   blocks stay ignored, and an empty or bad prompt is refused.

use serde_json::{json, Value};
#[cfg(feature = "shell-tools")]
use std::io::BufRead;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
#[cfg(feature = "shell-tools")]
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// 1x1 PNG header (enough for the sniffer).
const PNG: &[u8] = &[
    0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0, 0, 0, 0x0D, 0x49, 0x48, 0x44, 0x52, 0, 0, 0,
    1, 0, 0, 0, 1, 8, 6, 0, 0, 0, 0x1F, 0x15, 0xC4, 0x89,
];

/// Standard base64 (padded) — the test crate has no base64 dependency.
#[cfg(feature = "shell-tools")]
fn b64(data: &[u8]) -> String {
    const A: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    for c in data.chunks(3) {
        let n = (u32::from(c[0]) << 16)
            | (u32::from(*c.get(1).unwrap_or(&0)) << 8)
            | u32::from(*c.get(2).unwrap_or(&0));
        for i in 0..4 {
            if i <= c.len() {
                out.push(A[(n >> (18 - 6 * i) & 63) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

type Bodies = Arc<Mutex<Vec<Value>>>;

/// Mock provider: answers every request with a final `ok`, streamed or
/// not as asked, and records the request body.
fn spawn_mock() -> (String, Bodies) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let bodies: Bodies = Arc::new(Mutex::new(Vec::new()));
    let seen = Arc::clone(&bodies);
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let seen = Arc::clone(&seen);
            std::thread::spawn(move || {
                let mut s = stream;
                let mut buf = Vec::new();
                let mut chunk = [0u8; 16384];
                let mut body = Value::Null;
                let deadline = std::time::Instant::now() + Duration::from_secs(10);
                while std::time::Instant::now() < deadline {
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
                let streamed = body["stream"].as_bool() == Some(true);
                if body.get("messages").is_some() {
                    seen.lock().unwrap().push(body);
                }
                let resp = if streamed {
                    let mut out = String::new();
                    for part in ["ok", ""] {
                        out.push_str(&format!(
                            "data: {}\n\n",
                            json!({"choices": [{"index": 0,
                                "delta": {"content": part}, "finish_reason": null}]})
                        ));
                    }
                    out.push_str("data: [DONE]\n\n");
                    format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                        out.len(),
                        out
                    )
                } else {
                    let data = json!({
                        "id": "c", "object": "chat.completion", "created": 1, "model": "mock",
                        "choices": [{"index": 0, "message": {"role": "assistant",
                            "content": "ok"}, "finish_reason": "stop"}]
                    })
                    .to_string();
                    format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                        data.len(),
                        data
                    )
                };
                let _ = s.write_all(resp.as_bytes());
            });
        }
    });
    (format!("http://{addr}/v1"), bodies)
}

fn tmp(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!(
        "tole-img-turn-{tag}-{}-{:x}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&d).unwrap();
    d.canonicalize().unwrap()
}

fn base_cmd(url: &str, home: &Path, cwd: &Path) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_tole"));
    cmd.current_dir(cwd)
        .env("TOLE_BASE_URL", url)
        .env("TOLE_MODEL", "m")
        .env("TOLE_API_KEY", "k")
        .env("TOLE_NO_UPDATE_CHECK", "1")
        .env("CODECORA_HOME", home)
        .env("PATH", "/usr/bin:/bin")
        .env_remove("TOLE_VISION")
        .env_remove("TOLE_MODELS")
        .env_remove("TOLE_FALLBACK_BASE_URL")
        .env_remove("TOLE_MEMORY")
        .env_remove("TOLE_MEMORY_NAMESPACE")
        .env_remove("TOLE_SYSTEM_PROMPT")
        .env_remove("TOLE_TRUST")
        .env_remove("TOLE_AGENT_DEPTH");
    cmd
}

/// The `user` message payloads of the one session file in `dir`.
fn user_messages(dir: &Path) -> Vec<Value> {
    let file = std::fs::read_dir(dir)
        .unwrap()
        .flatten()
        .map(|e| e.path())
        .find(|p| p.extension().is_some_and(|x| x == "jsonl"))
        .expect("a session file");
    std::fs::read_to_string(file)
        .unwrap()
        .lines()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .flat_map(|v| match v {
            Value::Array(a) => a,
            one => vec![one],
        })
        .map(|v| v["payload"].clone())
        .filter(|p| p["role"] == "user")
        .collect()
}

/// The `image_url` parts of the last user message of a request body.
fn wire_images(body: &Value) -> usize {
    let last_user = body["messages"]
        .as_array()
        .unwrap()
        .iter()
        .rev()
        .find(|m| m["role"] == "user")
        .unwrap();
    last_user["content"]
        .as_array()
        .map(|parts| parts.iter().filter(|p| p["type"] == "image_url").count())
        .unwrap_or(0)
}

#[test]
fn chat_attaches_an_image_to_a_later_message() {
    let (url, bodies) = spawn_mock();
    let ws = tmp("chat");
    let home = tmp("chat-home");
    std::fs::write(ws.join("shot.png"), PNG).unwrap();
    let sessions = ws.join("sessions");
    let mut child = base_cmd(&url, &home, &ws)
        .args(["chat", "-s", sessions.to_str().unwrap()])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("run tole chat");
    child
        .stdin
        .take()
        .unwrap()
        .write_all(b"first\n/image ../outside.png\n/image shot.png\n/image\nsecond\nthird\n")
        .unwrap();
    let out = child.wait_with_output().unwrap();
    let (stdout, stderr) = (
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );
    assert!(out.status.success(), "{stdout}\n{stderr}");
    assert!(
        stderr.contains("/image:"),
        "a bad path is refused: {stderr}"
    );
    assert!(
        stdout.contains("image attached to your next message"),
        "{stdout}"
    );
    assert!(stdout.contains("1 image(s) queued"), "{stdout}");

    let users = user_messages(&sessions);
    let texts: Vec<_> = users.iter().map(|u| u["text"].clone()).collect();
    assert_eq!(texts, vec![json!("first"), json!("second"), json!("third")]);
    assert!(users[0].get("images").is_none(), "{:?}", users[0]);
    let imgs = users[1]["images"]
        .as_array()
        .expect("second turn has images");
    assert_eq!(imgs.len(), 1);
    assert_eq!(imgs[0]["mime"], "image/png");
    assert_eq!(
        imgs[0]["path"],
        json!(ws.join("shot.png").to_string_lossy())
    );
    assert!(
        users[2].get("images").is_none(),
        "taken by one message only"
    );

    // On the wire: only the second message carries the image part.
    let bodies = bodies.lock().unwrap();
    let per_turn: Vec<usize> = bodies.iter().map(wire_images).collect();
    assert_eq!(per_turn, vec![0, 1, 0], "{per_turn:?}");
    let _ = std::fs::remove_dir_all(&ws);
    let _ = std::fs::remove_dir_all(&home);
}

/// A line-delimited JSON-RPC client over the real `tole acp`.
#[cfg(feature = "shell-tools")]
struct Acp {
    child: std::process::Child,
    rx: mpsc::Receiver<String>,
}

#[cfg(feature = "shell-tools")]
impl Acp {
    fn spawn(url: &str, home: &Path, cwd: &Path) -> Self {
        let mut child = base_cmd(url, home, cwd)
            .arg("acp")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn tole acp");
        let stdout = child.stdout.take().unwrap();
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            for line in std::io::BufReader::new(stdout)
                .lines()
                .map_while(Result::ok)
            {
                if tx.send(line).is_err() {
                    break;
                }
            }
        });
        Self { child, rx }
    }

    fn call(&mut self, id: u64, method: &str, params: Value) -> Value {
        let msg = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
        writeln!(self.child.stdin.as_mut().unwrap(), "{msg}").unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(30);
        loop {
            let left = deadline.saturating_duration_since(std::time::Instant::now());
            let line = self
                .rx
                .recv_timeout(left)
                .unwrap_or_else(|_| panic!("no reply to {method} (id {id})"));
            if let Ok(v) = serde_json::from_str::<Value>(&line) {
                if v["id"].as_u64() == Some(id)
                    && (v.get("result").is_some() || v.get("error").is_some())
                {
                    return v;
                }
            }
        }
    }
}

#[cfg(feature = "shell-tools")]
impl Drop for Acp {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[cfg(feature = "shell-tools")]
#[test]
fn acp_image_blocks_are_spilled_recorded_and_image_only_prompts_run() {
    let (url, bodies) = spawn_mock();
    let cwd = tmp("acp");
    let home = tmp("acp-home");
    let mut acp = Acp::spawn(&url, &home, &cwd);

    let init = acp.call(1, "initialize", json!({"protocolVersion": 1}));
    assert_eq!(
        init["result"]["agentCapabilities"]["promptCapabilities"],
        json!({"image": true, "audio": false, "embeddedContext": false}),
        "{init}"
    );
    let created = acp.call(2, "session/new", json!({"cwd": cwd.to_string_lossy()}));
    let sid = created["result"]["sessionId"].as_str().unwrap().to_string();
    let png64 = b64(PNG);

    // 1. An image-only prompt runs (it used to be "missing prompt"). The
    //    declared MIME type is wrong on purpose: the bytes are sniffed.
    let r = acp.call(
        3,
        "session/prompt",
        json!({"sessionId": sid, "prompt": [
            {"type": "image", "mimeType": "image/jpeg", "data": png64,
             "uri": "file:///etc/passwd"}
        ]}),
    );
    assert_eq!(r["result"]["stopReason"], "end_turn", "{r}");

    // 2. Text plus an audio block: the audio is ignored, the text runs.
    let r = acp.call(
        4,
        "session/prompt",
        json!({"sessionId": sid, "prompt": [
            {"type": "text", "text": "and now?"},
            {"type": "audio", "mimeType": "audio/wav", "data": "UklGRg=="}
        ]}),
    );
    assert_eq!(r["result"]["stopReason"], "end_turn", "{r}");

    // 3. Refusals: nothing usable, or an image that is not one.
    let r = acp.call(
        5,
        "session/prompt",
        json!({"sessionId": sid, "prompt": [
            {"type": "audio", "mimeType": "audio/wav", "data": "UklGRg=="}
        ]}),
    );
    assert!(
        r["error"]["message"]
            .as_str()
            .is_some_and(|m| m.contains("missing prompt")),
        "{r}"
    );
    let not_png = b64(b"just text");
    let r = acp.call(
        6,
        "session/prompt",
        json!({"sessionId": sid, "prompt": [
            {"type": "text", "text": "x"},
            {"type": "image", "mimeType": "image/png", "data": not_png}
        ]}),
    );
    assert!(
        r["error"]["message"]
            .as_str()
            .is_some_and(|m| m.contains("not png/jpeg/gif/webp")),
        "{r}"
    );
    drop(acp);

    // The log holds the reference to the spilled file — never the blob.
    let sessions = cwd.join(".tole/sessions");
    let users = user_messages(&sessions);
    assert_eq!(users.len(), 2, "the refused prompts never ran: {users:?}");
    assert_eq!(users[0]["text"], "");
    let img = &users[0]["images"][0];
    assert_eq!(img["mime"], "image/png", "sniffed, not the declared jpeg");
    let spilled = PathBuf::from(img["path"].as_str().unwrap());
    assert!(spilled.starts_with(home.join("tole/images")), "{spilled:?}");
    assert_eq!(std::fs::read(&spilled).unwrap(), PNG);
    assert_eq!(users[1]["text"], "and now?");
    assert!(users[1].get("images").is_none());
    for f in std::fs::read_dir(&sessions).unwrap().flatten() {
        let raw = std::fs::read_to_string(f.path()).unwrap_or_default();
        assert!(!raw.contains(&png64), "the base64 blob must not be logged");
    }

    // On the wire: the image-only message is one image part, no empty text.
    let bodies = bodies.lock().unwrap();
    assert_eq!(bodies.len(), 2);
    let first = &bodies[0]["messages"]
        .as_array()
        .unwrap()
        .iter()
        .rev()
        .find(|m| m["role"] == "user")
        .unwrap()["content"];
    assert_eq!(first.as_array().unwrap().len(), 1, "{first}");
    assert_eq!(first[0]["type"], "image_url");
    assert_eq!(wire_images(&bodies[1]), 0, "the second message has none");
    let _ = std::fs::remove_dir_all(&cwd);
    let _ = std::fs::remove_dir_all(&home);
}
