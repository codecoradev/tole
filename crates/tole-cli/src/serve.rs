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
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{IpAddr, TcpListener, TcpStream};
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
    /// Explicit `--sessions-dir` override (None = per-session-cwd
    /// default, the pre-existing behavior).
    pub sessions_dir: Option<std::path::PathBuf>,
    /// `--on-turnend` stop gates wired onto every session registry.
    pub turnend: Vec<String>,
}

/// Max concurrently open connections: each pinned thread holds ~8 KiB
/// stack + a socket; capping bounds thread/socket exhaustion (CodeCora
/// scan-3 Wave-2 hardening).
const MAX_CONNECTIONS: usize = 32;
/// Failed-auth attempts allowed per source IP per window before the IP is
/// dropped until the window rolls over (simple fixed-window limiter).
const AUTH_WINDOW_SECS: u64 = 60;
const MAX_AUTH_FAILURES: u32 = 10;

struct State {
    sessions: SharedSessions,
    allow_patterns: Vec<String>,
    plan_mode: bool,
    memory: Option<MemoryConfig>,
    token: String,
    workspace_default: Option<String>,
    sessions_dir: Option<std::path::PathBuf>,
    turnend: Vec<String>,
    live_connections: std::sync::atomic::AtomicUsize,
    /// (window_start_epoch, failure_count) per source IP — fixed-window
    /// auth-failure limiter (brute-force hardening).
    auth_failures: Mutex<HashMap<IpAddr, (u64, u32)>>,
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
        sessions_dir: cfg.sessions_dir.clone(),
        turnend: cfg.turnend.clone(),
        live_connections: std::sync::atomic::AtomicUsize::new(0),
        auth_failures: Mutex::new(HashMap::new()),
    });
    eprintln!(
        "tole serve: listening on http://{addr} ({} allow pattern(s), plan_mode={})",
        cfg.allow_patterns.len(),
        cfg.plan_mode
    );
    use std::sync::atomic::Ordering;
    for stream in listener.incoming() {
        let Ok(stream) = stream else { continue };
        // Connection cap: refuse when at capacity (thread exhaustion DoS).
        if state.live_connections.load(Ordering::Relaxed) >= MAX_CONNECTIONS {
            drop(stream);
            continue;
        }
        let _ = stream.set_read_timeout(Some(SERVE_IO_TIMEOUT));
        let _ = stream.set_write_timeout(Some(SERVE_IO_TIMEOUT));
        state.live_connections.fetch_add(1, Ordering::Relaxed);
        let state = Arc::clone(&state);
        // One request per connection (Connection: close) — deliberately
        // simple: the client is curl/any HTTP client, not a browser.
        // ConnGuard (inside handle_conn) handles the decrement.
        std::thread::spawn(move || {
            handle_conn(stream, Arc::clone(&state));
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

/// Panic-safe connection-count guard: decrements on Drop (normal or
/// unwind), so the counter never permanently inflates after a panicking
/// handler.
struct ConnGuard(Arc<State>);
impl Drop for ConnGuard {
    fn drop(&mut self) {
        self.0
            .live_connections
            .fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
    }
}

fn handle_conn(mut stream: TcpStream, state: Arc<State>) {
    let _guard = ConnGuard(Arc::clone(&state));
    let Some(req) = read_request(&stream) else {
        return;
    };

    // Auth: bearer token on everything except /health (which carries no
    // information — it is a liveness probe). Failed auths are rate-limited
    // per source IP (CodeCora scan-3 Wave-2: brute-force hardening).
    let peer_ip = stream.peer_addr().ok().map(|a| a.ip());

    // Rate-limit: refuse when this IP has too many recent auth failures.
    // The lock is dropped before any respond call (CodeCora: a stalled
    // client must not hold the auth_failures mutex during a write).
    if let Some(ip) = peer_ip {
        let rate_limited = {
            let fails = state.auth_failures.lock().expect("auth failures lock");
            matches!(
                fails.get(&ip).map(|(window, count)| {
                    std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .map(|d| d.as_secs())
                        .unwrap_or(0)
                        .saturating_sub(*window)
                        <= AUTH_WINDOW_SECS
                        && *count >= MAX_AUTH_FAILURES
                }),
                Some(true)
            )
        };
        if rate_limited {
            respond(
                &mut stream,
                429,
                &json!({"error": "too many failed auth attempts"}),
            );
            return;
        }
    }

    let authorized = req.path == "/health"
        || req
            .authorization
            .as_deref()
            .and_then(|a| a.strip_prefix("Bearer "))
            .map(|t| t == state.token)
            .unwrap_or(false);
    if !authorized {
        if let Some(ip) = peer_ip {
            let mut fails = state.auth_failures.lock().expect("auth failures lock");
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0);
            // Periodic sweep: drop entries whose window has fully
            // expired — bounds memory growth from unique source IPs
            // (CodeCora scan round-2 on this PR).
            fails.retain(|_, (window, _)| now.saturating_sub(*window) <= AUTH_WINDOW_SECS * 4);
            let entry = fails.entry(ip).or_insert((now, 0));
            if now.saturating_sub(entry.0) > AUTH_WINDOW_SECS {
                entry.0 = now;
                entry.1 = 0;
            }
            entry.1 += 1;
        }
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
                state.sessions_dir.as_deref(),
                state.turnend.clone(),
                state.allow_patterns.clone(),
                // No REST cancel endpoint today (#178): a never-fired
                // token keeps serve behavior unchanged.
                tole_core::cancel::CancelToken::default(),
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
                            // try_lock on busy: a LOCKED busy means a turn
                            // is mid-flight on that session — skip it
                            // without blocking the whole map (CodeCora
                            // scan round-2). All-busy at capacity → 503.
                            let oldest = sessions
                                .map
                                .iter()
                                .filter(|(_, st)| {
                                    matches!(st.busy.try_lock().as_deref().copied(), Ok(false))
                                })
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
                    match run_session_turn(Arc::clone(&state.sessions), id, text, None) {
                        Ok((stop, Some(final_text))) => {
                            (200, json!({"stopReason": stop, "text": final_text}))
                        }
                        Ok((stop, None)) => (200, json!({"stopReason": stop})),
                        Err(e) if e.contains("session is busy") => (409, json!({"error": e})),
                        Err(e) => (500, json!({"error": e})),
                    }
                }
                ("POST", id, Some("cancel")) => {
                    // REST face of #178 (owner directive: serve follows
                    // the ACP design). Sets the session's cancel token;
                    // the in-flight prompt thread observes it at its
                    // next checkpoint and answers ITS caller with
                    // "cancelled". 404 only for an unknown session.
                    let cancelled = {
                        let sessions = lock_sessions(&state.sessions);
                        sessions.map.get(id).map(|st| st.cancel.cancel()).is_some()
                    };
                    if cancelled {
                        eprintln!("tole: session/{id} cancel requested (serve)");
                        (200, json!({"cancelled": true, "sessionId": id}))
                    } else {
                        (404, json!({"error": "unknown session"}))
                    }
                }
                _ => (404, json!({"error": "not found"})),
            }
        }
        _ => (404, json!({"error": "not found"})),
    }
}
