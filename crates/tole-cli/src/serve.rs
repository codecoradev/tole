//! D3 (issue #96): `tole serve` — tole as an HTTP daemon.
//!
//! A long-running, token-authenticated HTTP server exposing the session
//! host over REST:
//!
//! - `GET  /health`                → liveness (no auth)
//! - `POST /sessions`              → open a session (`{"cwd": "..."}`)
//! - `GET  /sessions`              → list session ids + busy flags
//! - `GET  /sessions/{id}`         → session status
//! - `POST /sessions/{id}/prompt`  → run one turn (`{"text": "..."}`)
//!
//! v1 scope: REST only (MCP-over-HTTP is the follow-up; the engine calls
//! are identical). Auth is a static bearer token (env `TOLE_SERVE_TOKEN`
//! or `--token`) — refuses to start without one. Binds 127.0.0.1 by
//! default; `--bind` overrides for trusted networks.
//!
//! HTTP is hand-rolled on std::net (tole's no-new-deps rule for host
//! plumbing): HTTP/1.1, Content-Length bodies only, one request per
//! connection (`Connection: close`). Runs turn-by-turn synchronously per
//! request; concurrent prompts on the same session get 409 (busy).

use anyhow::{Context, Result};
use serde_json::json;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tole_cli::session_host::{
    lock_sessions, open_session, run_session_turn, Sessions, SharedSessions,
};
use tole_core::memory::MemoryConfig;
use tole_core::storage::Storage;

pub struct ServeConfig {
    pub bind: String,
    pub port: u16,
    pub token: Option<String>,
    pub allow_patterns: Vec<String>,
    pub workspace: Option<String>,
    pub plan_mode: bool,
    pub memory: Option<MemoryConfig>,
}

struct State {
    sessions: SharedSessions,
    allow_patterns: Vec<String>,
    plan_mode: bool,
    memory: Option<MemoryConfig>,
    token: String,
    workspace_default: Option<String>,
}

/// Read/write ceilings per connection: a client that opens a socket and
/// never speaks must not pin a thread forever (CodeCora scan
/// 2026-09-28, thread-exhaustion DoS). Generous for real clients.
const SERVE_IO_TIMEOUT: Duration = Duration::from_secs(30);

pub fn run_serve(cfg: ServeConfig) -> Result<()> {
    let token = resolve_token(&cfg.token)?;
    let addr = format!("{}:{}", cfg.bind, cfg.port);
    let listener = TcpListener::bind(&addr)
        .with_context(|| format!("binding {addr} (in use? change --port)"))?;
    let state = Arc::new(State {
        sessions: Arc::new(Mutex::new(Sessions::default())),
        allow_patterns: cfg.allow_patterns.clone(),
        plan_mode: cfg.plan_mode,
        memory: cfg.memory.clone(),
        token,
        workspace_default: cfg.workspace.clone(),
    });
    eprintln!(
        "tole serve: listening on http://{addr} ({} allow pattern(s), plan_mode={})",
        cfg.allow_patterns.len(),
        cfg.plan_mode
    );
    for stream in listener.incoming() {
        let Ok(stream) = stream else { continue };
        let _ = stream.set_read_timeout(Some(SERVE_IO_TIMEOUT));
        let _ = stream.set_write_timeout(Some(SERVE_IO_TIMEOUT));
        let state = Arc::clone(&state);
        // One request per connection (Connection: close) — deliberately
        // simple: the client is curl/any HTTP client, not a browser.
        std::thread::spawn(move || {
            handle_conn(stream, state);
        });
    }
    Ok(())
}

fn resolve_token(flag: &Option<String>) -> Result<String> {
    if let Some(t) = flag.as_deref().filter(|t| !t.trim().is_empty()) {
        return Ok(t.trim().to_string());
    }
    if let Ok(t) = std::env::var("TOLE_SERVE_TOKEN") {
        let t = t.trim().to_string();
        if !t.is_empty() {
            return Ok(t);
        }
    }
    anyhow::bail!("refusing to start an unauthenticated server: set --token or TOLE_SERVE_TOKEN")
}

// ---------------------------------------------------------------------------
// Connection handling
// ---------------------------------------------------------------------------

struct Request {
    method: String,
    path: String,
    authorization: Option<String>,
    body: String,
}

/// Ceiling for the request line + all header bytes combined: a client
/// that streams an endless line without a newline must not buffer
/// gigabytes pre-auth (CodeCora scan-3: unbounded buffering).
const MAX_HEADER_BYTES: usize = 32 * 1024;

fn read_request(stream: &TcpStream) -> Option<Request> {
    let mut reader = BufReader::new(stream);
    let mut request_line = String::new();
    // take() bounds the read: an over-long line arrives truncated with no
    // trailing newline, which the caller treats as a protocol error.
    reader
        .by_ref()
        .take(MAX_HEADER_BYTES as u64)
        .read_line(&mut request_line)
        .ok()?;
    if !request_line.ends_with('\n') {
        return None;
    }
    let mut parts = request_line.split_whitespace();
    let method = parts.next()?.to_string();
    let path = parts.next()?.to_string();
    let mut authorization = None;
    let mut content_length = 0usize;
    let mut header_bytes = 0usize;
    loop {
        if header_bytes > MAX_HEADER_BYTES {
            return None;
        }
        let mut line = String::new();
        let n = reader
            .by_ref()
            .take((MAX_HEADER_BYTES - header_bytes) as u64)
            .read_line(&mut line)
            .ok()?;
        if n == 0 {
            return None;
        }
        header_bytes += n;
        if !line.ends_with('\n') {
            return None;
        }
        let line = line.trim_end();
        if line.is_empty() {
            break;
        }
        let lower = line.to_ascii_lowercase();
        if let Some(v) = lower.strip_prefix("content-length:") {
            content_length = v.trim().parse().unwrap_or(0);
        }
        // Authorization keeps its ORIGINAL case (the bearer token is
        // case-sensitive — a lowercased copy never matches).
        if let Some(v) = line.strip_prefix("Authorization:") {
            authorization = Some(v.trim().to_string());
        }
    }
    if content_length > 1024 * 1024 {
        return None; // body too large — refuse rather than buffer it
    }
    let mut body = vec![0u8; content_length];
    if content_length > 0 {
        reader.read_exact(&mut body).ok()?;
    }
    Some(Request {
        method,
        path,
        authorization,
        body: String::from_utf8_lossy(&body).to_string(),
    })
}

fn respond(stream: &mut TcpStream, status: u16, payload: &serde_json::Value) {
    let body = payload.to_string();
    let reason = match status {
        200 => "OK",
        400 => "Bad Request",
        401 => "Unauthorized",
        404 => "Not Found",
        405 => "Method Not Allowed",
        409 => "Conflict",
        413 => "Payload Too Large",
        500 => "Internal Server Error",
        _ => "OK",
    };
    let head = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    let _ = stream.write_all(head.as_bytes());
    let _ = stream.write_all(body.as_bytes());
    let _ = stream.flush();
}

fn handle_conn(mut stream: TcpStream, state: Arc<State>) {
    let Some(req) = read_request(&stream) else {
        return;
    };

    // Auth: bearer token on everything except /health (which carries no
    // information — it is a liveness probe).
    let authorized = req.path == "/health"
        || req
            .authorization
            .as_deref()
            .and_then(|a| a.strip_prefix("Bearer "))
            .map(|t| t == state.token)
            .unwrap_or(false);
    if !authorized {
        respond(&mut stream, 401, &json!({"error": "unauthorized"}));
        return;
    }

    let (status, payload) = route(&state, &req.method, &req.path, &req.body);
    respond(&mut stream, status, &payload);
}

fn route(state: &State, method: &str, path: &str, body: &str) -> (u16, serde_json::Value) {
    match (method, path) {
        ("GET", "/health") => (200, json!({"status": "ok"})),
        ("GET", "/sessions") => {
            let sessions = lock_sessions(&state.sessions);
            let list: Vec<_> = sessions
                .map
                .iter()
                .map(|(id, st)| {
                    json!({
                        "id": id,
                        "busy": *st.busy.lock().expect("busy lock"),
                    })
                })
                .collect();
            (200, json!({"sessions": list}))
        }
        ("POST", "/sessions") => {
            let Ok(req) = serde_json::from_str::<serde_json::Value>(body) else {
                return (400, json!({"error": "invalid JSON body"}));
            };
            let cwd = req
                .get("cwd")
                .and_then(|v| v.as_str())
                .unwrap_or(state.workspace_default.as_deref().unwrap_or("."));
            let session_id = tole_cli::session_host::new_session_id("srv");
            let approver =
                tole_core::approval::AllowlistApprover::allow_only(state.allow_patterns.clone());
            match open_session(
                &session_id,
                cwd,
                false,
                state.plan_mode,
                approver,
                state.memory.clone(),
            ) {
                Ok(session_state) => {
                    {
                        // Cap the live map: a long-running daemon must not
                        // grow without bound (CodeCora scan-3). Sessions
                        // are durable on disk (the JSONL file) — eviction
                        // drops only the in-memory handle, never the
                        // file; busy sessions are never evicted.
                        const MAX_SESSIONS: usize = 256;
                        let mut sessions = lock_sessions(&state.sessions);
                        while sessions.map.len() >= MAX_SESSIONS {
                            let oldest = sessions
                                .map
                                .iter()
                                .filter(|(_, st)| !*st.busy.lock().expect("busy lock"))
                                .map(|(id, _)| id.clone())
                                .next();
                            match oldest {
                                Some(id) => {
                                    sessions.map.remove(&id);
                                }
                                None => {
                                    return (
                                        503,
                                        json!({"error": "all sessions busy — at capacity"}),
                                    )
                                }
                            }
                        }
                        sessions.map.insert(session_id.clone(), session_state);
                    }
                    (200, json!({"sessionId": session_id, "workspace": cwd}))
                }
                Err(e) => (500, json!({"error": e})),
            }
        }
        (method, path) if path.starts_with("/sessions/") => {
            let rest = &path["/sessions/".len()..];
            // Split once; a bare id (no trailing slash) is the status route.
            let (id, action) = match rest.split_once('/') {
                Some((id, action)) => (id, Some(action)),
                None => (rest, None),
            };
            match (method, id, action) {
                ("GET", id, None | Some("status")) => {
                    // Take the storage HANDLE under the map lock, then
                    // release it before touching storage: a running turn
                    // holds its storage lock for the whole LLM call, and
                    // blocking status on it would pin the endpoint (and
                    // the map) for the entire turn (CodeCora scan
                    // 2026-09-28). try_lock keeps this non-blocking.
                    let (busy, entries) = {
                        let sessions = lock_sessions(&state.sessions);
                        let Some(st) = sessions.map.get(id) else {
                            return (404, json!({"error": "unknown session"}));
                        };
                        let busy = *st.busy.lock().expect("busy lock");
                        let entries = match st.storage.try_lock() {
                            Ok(guard) => Some(guard.entries().len()),
                            Err(_) => None, // turn in flight — skip the count
                        };
                        (busy, entries)
                    };
                    let entries = match entries {
                        Some(n) => json!(n),
                        None => json!(null),
                    };
                    (
                        200,
                        json!({
                            "id": id,
                            "entries": entries,
                            "busy": busy,
                        }),
                    )
                }
                ("POST", id, Some("prompt")) => {
                    let Ok(req) = serde_json::from_str::<serde_json::Value>(body) else {
                        return (400, json!({"error": "invalid JSON body"}));
                    };
                    let Some(text) = req.get("text").and_then(|v| v.as_str()) else {
                        return (400, json!({"error": "missing text"}));
                    };
                    match run_session_turn(Arc::clone(&state.sessions), id, text) {
                        Ok((stop, Some(final_text))) => {
                            (200, json!({"stopReason": stop, "text": final_text}))
                        }
                        Ok((stop, None)) => (200, json!({"stopReason": stop})),
                        Err(e) if e.contains("session is busy") => (409, json!({"error": e})),
                        Err(e) => (500, json!({"error": e})),
                    }
                }
                _ => (404, json!({"error": "not found"})),
            }
        }
        _ => (404, json!({"error": "not found"})),
    }
}
