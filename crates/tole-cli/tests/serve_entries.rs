//! #524 transcript read and resume-by-id — end to end over the REAL serve
//! face. `GET /sessions/{id}/entries` pages through exactly what the
//! session's JSONL replays, needs the token, refuses bad queries, and keeps
//! working while a turn is in flight (it reads the file, not the storage
//! lock a running turn holds). `POST /sessions {"resume": id}` makes a
//! session live again after a restart, never disturbs a live one, and
//! respects the jail and the writer lock.

// Spawns the real `tole serve` binary (`shell-tools` feature, #349).
#![cfg(feature = "shell-tools")]

use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

mod common;

struct Serve {
    child: Child,
    port: u16,
    dir: std::path::PathBuf,
    /// Remove `dir` on drop (only the process that created it).
    own_dir: bool,
}

impl Serve {
    fn spawn(base: &str) -> Self {
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
    fn spawn_in(base: &str, dir: std::path::PathBuf, own_dir: bool) -> Self {
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
    fn kill(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }

    fn http_with(&self, method: &str, path: &str, auth: bool, body: Option<&str>) -> (u16, Value) {
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

    fn http(&self, method: &str, path: &str, body: Option<&str>) -> (u16, Value) {
        self.http_with(method, path, true, body)
    }

    fn session_file(&self, id: &str) -> std::path::PathBuf {
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
/// whose last user message contains "slow" is answered after `slow_ms`.
fn spawn_mock(slow_ms: u64) -> String {
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
                let resp = if streamed {
                    let out = format!(
                        "data: {}\n\ndata: {}\n\ndata: [DONE]\n\n",
                        json!({"choices": [{"index": 0, "delta": {"content": text},
                            "finish_reason": null}]}),
                        json!({"choices": [], "usage": {"prompt_tokens": 1,
                            "completion_tokens": 1, "total_tokens": 2}})
                    );
                    format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{out}",
                        out.len()
                    )
                } else {
                    let data = json!({
                        "id": "c", "object": "chat.completion", "created": 1, "model": "mock",
                        "choices": [{"index": 0, "message": {"role": "assistant",
                            "content": text}, "finish_reason": "stop"}],
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

fn new_session(serve: &Serve) -> String {
    let (code, body) = serve.http("POST", "/sessions", Some("{}"));
    assert_eq!(code, 200, "{body}");
    body["sessionId"].as_str().unwrap().to_string()
}

fn prompt(serve: &Serve, id: &str, text: &str) {
    let (code, body) = serve.http(
        "POST",
        &format!("/sessions/{id}/prompt"),
        Some(&json!({"text": text}).to_string()),
    );
    assert_eq!(code, 200, "{body}");
}

/// The page sequence equals what `tole resume` replays from the file.
#[test]
fn paging_through_entries_equals_the_replayed_log() {
    let serve = Serve::spawn(&spawn_mock(0));
    let id = new_session(&serve);
    prompt(&serve, &id, "first");
    prompt(&serve, &id, "second");

    let replayed: Vec<Value> = tole_core::storage::Storage::entries(
        &tole_core::storage::JsonlStorage::open_read_only(serve.session_file(&id)).unwrap(),
    )
    .iter()
    .map(|e| serde_json::to_value(e).unwrap())
    .collect();
    assert!(replayed.len() >= 4, "two turns leave several entries");

    // Page two at a time, following nextAfter.
    let mut after = 0u64;
    let mut got: Vec<Value> = Vec::new();
    loop {
        let (code, page) = serve.http(
            "GET",
            &format!("/sessions/{id}/entries?after={after}&limit=2"),
            None,
        );
        assert_eq!(code, 200, "{page}");
        assert_eq!(page["id"], id);
        got.extend(page["entries"].as_array().unwrap().iter().cloned());
        assert_eq!(page["lastSeq"], replayed.last().unwrap()["seq"]);
        if page["hasMore"] != json!(true) {
            break;
        }
        let next = page["nextAfter"].as_u64().unwrap();
        assert!(next > after, "pagination must advance");
        after = next;
    }
    assert_eq!(got, replayed);

    // No query at all = the default page from the start.
    let (code, page) = serve.http("GET", &format!("/sessions/{id}/entries"), None);
    assert_eq!(code, 200);
    assert_eq!(page["entries"].as_array().unwrap().len(), replayed.len());
    // The existing status route is unaffected by the query-string split.
    let (code, st) = serve.http("GET", &format!("/sessions/{id}/status"), None);
    assert_eq!(code, 200);
    assert_eq!(st["entries"], json!(replayed.len()));
}

#[test]
fn entries_needs_the_token_and_refuses_bad_input() {
    let serve = Serve::spawn(&spawn_mock(0));
    let id = new_session(&serve);
    let route = format!("/sessions/{id}/entries");
    assert_eq!(serve.http_with("GET", &route, false, None).0, 401);
    assert_eq!(serve.http("GET", "/sessions/srv-nope/entries", None).0, 404);
    for q in ["after=x", "after=-1", "limit=0", "limit=many"] {
        let (code, body) = serve.http("GET", &format!("{route}?{q}"), None);
        assert_eq!(code, 400, "{q}: {body}");
    }
    // A POST to the same path is not a read route.
    assert_ne!(serve.http("POST", &route, Some("{}")).0, 200);
    // `limit` above the ceiling is clamped, not an error.
    assert_eq!(
        serve.http("GET", &format!("{route}?limit=999999"), None).0,
        200
    );
}

/// The reason for reading the file: a turn in flight holds the storage
/// lock, and the transcript must stay readable meanwhile.
#[test]
fn the_transcript_is_readable_while_a_turn_is_running() {
    let serve = Serve::spawn(&spawn_mock(3000));
    let id = new_session(&serve);
    prompt(&serve, &id, "before"); // some history exists

    let port = serve.port;
    let id2 = id.clone();
    let turn = std::thread::spawn(move || {
        // Raw request: the slow turn blocks this thread for ~3 s.
        let mut s = std::net::TcpStream::connect(("127.0.0.1", port)).unwrap();
        s.set_read_timeout(Some(Duration::from_secs(20))).unwrap();
        let body = json!({"text": "slow please"}).to_string();
        write!(
            s,
            "POST /sessions/{id2}/prompt HTTP/1.1\r\nAuthorization: Bearer t0ken\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        )
        .unwrap();
        let mut out = String::new();
        let _ = s.read_to_string(&mut out);
        out
    });

    // Wait until the turn is really running.
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let (_, list) = serve.http("GET", "/sessions", None);
        if list["sessions"][0]["busy"] == json!(true) {
            break;
        }
        assert!(Instant::now() < deadline, "turn never became busy");
        std::thread::sleep(Duration::from_millis(50));
    }
    let started = Instant::now();
    let (code, page) = serve.http("GET", &format!("/sessions/{id}/entries"), None);
    assert_eq!(code, 200, "{page}");
    assert!(
        started.elapsed() < Duration::from_millis(1500),
        "the read must not wait for the turn (took {:?})",
        started.elapsed()
    );
    // The in-flight turn's user message is already durable and visible.
    let texts: Vec<String> = page["entries"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|e| e["payload"]["text"].as_str().map(str::to_string))
        .collect();
    assert!(texts.iter().any(|t| t == "before"), "{texts:?}");
    let reply = turn.join().unwrap();
    assert!(reply.contains("200"), "{reply}");
}

fn entry_count(serve: &Serve, id: &str) -> usize {
    let (code, page) = serve.http("GET", &format!("/sessions/{id}/entries?limit=500"), None);
    assert_eq!(code, 200, "{page}");
    page["entries"].as_array().unwrap().len()
}

/// A session is not live after a restart; resume-by-id brings it back with
/// its history, and a new turn extends the same log.
#[test]
fn resume_brings_a_session_back_after_a_restart() {
    let base = spawn_mock(0);
    let mut first = Serve::spawn(&base);
    let id = new_session(&first);
    prompt(&first, &id, "before the restart");
    let before = entry_count(&first, &id);
    assert!(before >= 2);
    first.kill();

    let second = Serve::spawn_in(&base, first.dir.clone(), false);
    // Not live yet: the transcript route has nothing to read it from.
    assert_eq!(
        second
            .http("GET", &format!("/sessions/{id}/entries"), None)
            .0,
        404
    );
    let (code, body) = second.http(
        "POST",
        "/sessions",
        Some(&json!({"resume": id}).to_string()),
    );
    assert_eq!(code, 200, "{body}");
    assert_eq!(body["sessionId"], id);
    assert_eq!(body["resumed"], true);
    assert_eq!(body["alreadyLive"], false);
    assert_eq!(entry_count(&second, &id), before, "history is intact");

    // It is a working session again: a new turn appends to the same log.
    prompt(&second, &id, "after the restart");
    assert!(entry_count(&second, &id) > before);
    let (_, list) = second.http("GET", "/sessions", None);
    assert_eq!(list["sessions"].as_array().unwrap().len(), 1);
}

/// Resuming a live session changes nothing: it is not reopened (the lock
/// is held by this very process) and a running turn is not disturbed.
#[test]
fn resuming_a_live_session_is_a_no_op_even_mid_turn() {
    let serve = Serve::spawn(&spawn_mock(2500));
    let id = new_session(&serve);
    let resume = json!({"resume": id}).to_string();

    let (code, body) = serve.http("POST", "/sessions", Some(&resume));
    assert_eq!(code, 200, "{body}");
    assert_eq!(
        (body["resumed"].clone(), body["alreadyLive"].clone()),
        (json!(false), json!(true))
    );
    assert_eq!(body["busy"], false);

    let port = serve.port;
    let id2 = id.clone();
    let turn = std::thread::spawn(move || {
        let mut s = std::net::TcpStream::connect(("127.0.0.1", port)).unwrap();
        s.set_read_timeout(Some(Duration::from_secs(20))).unwrap();
        let body = json!({"text": "slow please"}).to_string();
        write!(
            s,
            "POST /sessions/{id2}/prompt HTTP/1.1\r\nAuthorization: Bearer t0ken\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        )
        .unwrap();
        let mut out = String::new();
        let _ = s.read_to_string(&mut out);
        out
    });
    let deadline = Instant::now() + Duration::from_secs(10);
    while serve.http("GET", "/sessions", None).1["sessions"][0]["busy"] != json!(true) {
        assert!(Instant::now() < deadline, "turn never became busy");
        std::thread::sleep(Duration::from_millis(50));
    }
    let (code, body) = serve.http("POST", "/sessions", Some(&resume));
    assert_eq!(code, 200, "{body}");
    assert_eq!(
        (body["alreadyLive"].clone(), body["busy"].clone()),
        (json!(true), json!(true))
    );
    // The turn still finishes normally.
    assert!(turn.join().unwrap().contains("ENTRIES-ANSWER"));
}

#[test]
fn resume_refuses_bad_ids_unknown_sessions_other_dirs_and_a_held_lock() {
    let base = spawn_mock(0);
    let mut first = Serve::spawn(&base);
    let id = new_session(&first);
    prompt(&first, &id, "hi");
    let post = |s: &Serve, v: Value| s.http("POST", "/sessions", Some(&v.to_string()));

    // Malformed / unknown / wrong type.
    assert_eq!(post(&first, json!({"resume": "../etc/passwd"})).0, 400);
    assert_eq!(post(&first, json!({"resume": ""})).0, 400);
    assert_eq!(post(&first, json!({"resume": 7})).0, 400);
    assert_eq!(post(&first, json!({"resume": "srv-nope"})).0, 404);
    // The jail still applies: a cwd outside the workspace is refused even
    // when the id is valid.
    assert_eq!(post(&first, json!({"resume": id, "cwd": "/"})).0, 403);
    // A second serve process over the same workspace while the first still
    // holds the session's writer lock: refused, nothing opened.
    let second = Serve::spawn_in(&base, first.dir.clone(), false);
    let (code, body) = post(&second, json!({"resume": id}));
    assert_eq!(code, 409, "{body}");
    assert!(
        body["error"].as_str().unwrap().contains("open for writing"),
        "{body}"
    );
    let (_, list) = second.http("GET", "/sessions", None);
    assert_eq!(list["sessions"].as_array().unwrap().len(), 0);
    // Once the holder is gone the same call succeeds.
    first.kill();
    // A session lives under ITS workspace: another (jailed) directory has no
    // such file, so a session that is not live is unknown there.
    std::fs::create_dir_all(first.dir.join("sub")).unwrap();
    let sub = first.dir.join("sub").to_string_lossy().into_owned();
    assert_eq!(post(&second, json!({"resume": id, "cwd": sub})).0, 404);

    assert_eq!(post(&second, json!({"resume": id})).0, 200);
}
