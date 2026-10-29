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
//! - `POST /sessions/{id}/interrupt` → steer the running turn without
//!   cancelling it (`{"text": "...", "urgent": false}`, #377)
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
use std::time::{Duration, Instant};

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

struct State {
    sessions: SharedSessions,
    /// Remote approval queue (issue #200): pending Write decisions from
    /// every session, decided via /approvals routes.
    approvals: Arc<tole_cli::approvals::ApprovalQueue>,
    allow_patterns: Vec<String>,
    plan_mode: bool,
    memory: Option<MemoryConfig>,
    token: String,
    /// Jail-of-jails root (canonical): `--workspace`, else the server
    /// cwd. Client-supplied session cwds must resolve inside it.
    workspace_root: std::path::PathBuf,
    sessions_dir: Option<std::path::PathBuf>,
    turnend: Vec<String>,
    live_connections: std::sync::atomic::AtomicUsize,
    /// Per-source-IP failed-auth limiter (brute-force hardening), shared
    /// logic with the MCP transport.
    auth_failures: tole_cli::auth::AuthLimiter,
}

/// Read/write ceilings per connection: a client that opens a socket and
/// never speaks must not pin a thread forever (CodeCora scan
/// 2026-09-28, thread-exhaustion DoS). Generous for real clients.
const SERVE_IO_TIMEOUT: Duration = Duration::from_secs(30);
/// Total budget for reading one request (request line, headers and body).
/// The per-read timeout alone lets a client that trickles one byte every
/// few seconds pin a connection slot indefinitely (slow-loris); this
/// deadline bounds the whole read.
const REQUEST_READ_DEADLINE: Duration = Duration::from_secs(15);

pub fn run_serve(cfg: ServeConfig) -> Result<()> {
    let token = resolve_token(&cfg.token)?;
    let workspace_root = crate::resolve_workspace_root(cfg.workspace.as_ref())
        .context("resolving --workspace for the session jail root")?;
    let addr = format!("{}:{}", cfg.bind, cfg.port);
    let listener = TcpListener::bind(&addr)
        .with_context(|| format!("binding {addr} (in use? change --port)"))?;
    let state = Arc::new(State {
        sessions: Arc::new(Mutex::new(Sessions::default())),
        approvals: tole_cli::approvals::ApprovalQueue::shared(),
        allow_patterns: cfg.allow_patterns.clone(),
        plan_mode: cfg.plan_mode,
        memory: cfg.memory.clone(),
        token,
        workspace_root,
        sessions_dir: cfg.sessions_dir.clone(),
        turnend: cfg.turnend.clone(),
        live_connections: std::sync::atomic::AtomicUsize::new(0),
        auth_failures: tole_cli::auth::AuthLimiter::new(),
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

/// Why a request could not be read; decides the response (if any).
enum ReadError {
    /// Connection-level failure or timeout: nothing useful to answer.
    Io,
    /// Syntactically unacceptable request: answered with 400.
    Bad(&'static str),
    /// Declared body over the ceiling: answered with 413.
    TooLarge,
}

/// Ceiling for the request line + all header bytes combined: a client
/// that streams an endless line without a newline must not buffer
/// gigabytes pre-auth (CodeCora scan-3: unbounded buffering).
const MAX_HEADER_BYTES: usize = 32 * 1024;
/// Ceiling for a request body.
const MAX_BODY_BYTES: usize = 1024 * 1024;

/// A reader that re-arms the socket read timeout before every read so the
/// remaining time until `deadline` — not just the per-read timeout —
/// bounds the whole request read.
struct DeadlineReader<'a> {
    stream: &'a TcpStream,
    deadline: Instant,
}

impl Read for DeadlineReader<'_> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let remaining = self.deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(std::io::ErrorKind::TimedOut.into());
        }
        self.stream
            .set_read_timeout(Some(remaining.min(SERVE_IO_TIMEOUT)))?;
        self.stream.read(buf)
    }
}

fn read_request(stream: &TcpStream) -> Result<Request, ReadError> {
    let mut reader = BufReader::new(DeadlineReader {
        stream,
        deadline: Instant::now() + REQUEST_READ_DEADLINE,
    });
    let mut request_line = String::new();
    // take() bounds the read: an over-long line arrives truncated with no
    // trailing newline, which the caller treats as a protocol error.
    reader
        .by_ref()
        .take(MAX_HEADER_BYTES as u64)
        .read_line(&mut request_line)
        .map_err(|_| ReadError::Io)?;
    if !request_line.ends_with('\n') {
        return Err(ReadError::Io);
    }
    let mut parts = request_line.split_whitespace();
    let method = parts
        .next()
        .ok_or(ReadError::Bad("malformed request line"))?;
    let path = parts
        .next()
        .ok_or(ReadError::Bad("malformed request line"))?;
    let (method, path) = (method.to_string(), path.to_string());
    let mut authorization = None;
    let mut content_length: Option<usize> = None;
    let mut expect_continue = false;
    // The request line counts against the same ceiling as the headers:
    // the documented limit is 32 KiB for both combined.
    let mut header_bytes = request_line.len();
    loop {
        if header_bytes > MAX_HEADER_BYTES {
            return Err(ReadError::Io);
        }
        let mut line = String::new();
        let n = reader
            .by_ref()
            .take((MAX_HEADER_BYTES - header_bytes) as u64)
            .read_line(&mut line)
            .map_err(|_| ReadError::Io)?;
        if n == 0 {
            return Err(ReadError::Io);
        }
        header_bytes += n;
        if !line.ends_with('\n') {
            return Err(ReadError::Io);
        }
        let line = line.trim_end();
        if line.is_empty() {
            break;
        }
        // Header names are case-insensitive (RFC 9110 §5.1); values keep
        // their original case (the bearer token is case-sensitive).
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        let value = value.trim();
        if name.eq_ignore_ascii_case("content-length") {
            // Two Content-Length headers (or an unparsable one) are a
            // request-smuggling hazard: reject instead of last-wins.
            if content_length.is_some() {
                return Err(ReadError::Bad("duplicate Content-Length"));
            }
            content_length = Some(
                value
                    .parse()
                    .map_err(|_| ReadError::Bad("invalid Content-Length"))?,
            );
        } else if name.eq_ignore_ascii_case("transfer-encoding") {
            // Chunked bodies are not supported; guessing a framing is
            // exactly what smuggling relies on.
            return Err(ReadError::Bad("Transfer-Encoding is not supported"));
        } else if name.eq_ignore_ascii_case("authorization") {
            authorization = Some(value.to_string());
        } else if name.eq_ignore_ascii_case("expect") && value.eq_ignore_ascii_case("100-continue")
        {
            expect_continue = true;
        }
    }
    let content_length = content_length.unwrap_or(0);
    if content_length > MAX_BODY_BYTES {
        return Err(ReadError::TooLarge); // refuse rather than buffer it
    }
    if content_length > 0 && expect_continue {
        // curl sends `Expect: 100-continue` for bodies over ~1 KiB and
        // waits ~1s for this interim response before sending the body.
        let mut w = stream;
        let _ = w.write_all(b"HTTP/1.1 100 Continue\r\n\r\n");
        let _ = w.flush();
    }
    let mut body = vec![0u8; content_length];
    if content_length > 0 {
        reader.read_exact(&mut body).map_err(|_| ReadError::Io)?;
    }
    Ok(Request {
        method,
        path,
        authorization,
        body: String::from_utf8_lossy(&body).to_string(),
    })
}

fn reason_phrase(status: u16) -> &'static str {
    match status {
        200 => "OK",
        202 => "Accepted",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        409 => "Conflict",
        413 => "Payload Too Large",
        429 => "Too Many Requests",
        500 => "Internal Server Error",
        503 => "Service Unavailable",
        _ => "Unknown",
    }
}

fn respond(stream: &mut TcpStream, status: u16, payload: &serde_json::Value) {
    let body = payload.to_string();
    let reason = reason_phrase(status);
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
    let req = match read_request(&stream) {
        Ok(r) => r,
        Err(ReadError::Io) => return,
        Err(ReadError::Bad(msg)) => {
            respond(&mut stream, 400, &json!({"error": msg}));
            return;
        }
        Err(ReadError::TooLarge) => {
            respond(
                &mut stream,
                413,
                &json!({"error": "request body too large"}),
            );
            return;
        }
    };

    // Auth: bearer token on everything except /health (which carries no
    // information — it is a liveness probe). Failed auths are rate-limited
    // per source IP (CodeCora scan-3 Wave-2: brute-force hardening).
    let peer_ip = stream.peer_addr().ok().map(|a| a.ip());

    // Rate-limit: refuse when this IP has too many recent auth failures.
    if peer_ip.is_some_and(|ip| state.auth_failures.is_limited(ip)) {
        respond(
            &mut stream,
            429,
            &json!({"error": "too many failed auth attempts"}),
        );
        return;
    }

    let authorized = req.path == "/health"
        || tole_cli::auth::bearer_matches(req.authorization.as_deref(), &state.token);
    if !authorized {
        if let Some(ip) = peer_ip {
            state.auth_failures.record_failure(ip);
        }
        respond(&mut stream, 401, &json!({"error": "unauthorized"}));
        return;
    }

    let (status, payload) = route(&state, &req.method, &req.path, &req.body);
    respond(&mut stream, status, &payload);
}

/// Write the decision audit register onto the session's durable log:
/// via the in-memory handle when present (blocking lock), reopening the
/// JSONL from disk when the session was evicted. Errors propagate.
fn write_approval_audit(
    state: &State,
    session_id: &str,
    approval_id: &str,
    allow: bool,
    tool: &str,
) -> Result<(), String> {
    use tole_core::register::RegisterWrite;
    use tole_core::storage::{Commit, JsonlStorage, Storage};
    let register = RegisterWrite::set(
        "fact",
        format!("approval_{approval_id}"),
        json!({
            "decision": if allow { "allow" } else { "deny" },
            "tool": tool,
            "approvalId": approval_id,
            "decidedAtMs": std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis() as u64)
                .unwrap_or(0),
        }),
    );
    // In-memory handle: clone the storage Arc under the map lock, then
    // RELEASE the map lock before touching the storage mutex — a turn
    // holds that lock for its whole duration, and freezing the global
    // map would stall every route (cora MAJOR).
    let handle = {
        let sessions = lock_sessions(&state.sessions);
        sessions
            .get(session_id)
            .map(|st| std::sync::Arc::clone(&st.storage))
    };
    if let Some(storage) = handle {
        let mut storage = storage.lock().unwrap_or_else(|p| p.into_inner());
        return storage
            .commit(Commit::new().register(register))
            .map(|_| ())
            .map_err(|e| e.to_string());
    }
    // Evicted: reopen the queue-time durable path — the EXACT file the
    // session was created with (cwd-dependent layouts defeat daemon-cwd
    // guesses, cora MAJOR).
    let path = state
        .approvals
        .storage_path(session_id)
        .ok_or_else(|| format!("no queued approval for session {session_id}"))?;
    let mut storage = JsonlStorage::open(path).map_err(|e| e.to_string())?;
    storage
        .commit(Commit::new().register(register))
        .map(|_| ())
        .map_err(|e| e.to_string())
}

fn route(state: &State, method: &str, path: &str, body: &str) -> (u16, serde_json::Value) {
    match (method, path) {
        ("GET", "/health") => (200, json!({"status": "ok"})),
        ("GET", "/sessions") => {
            let sessions = lock_sessions(&state.sessions);
            let list: Vec<_> = sessions
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
            let requested = match req.get("cwd").and_then(|v| v.as_str()) {
                Some(c) => c.to_string(),
                None => state.workspace_root.to_string_lossy().into_owned(),
            };
            // Jail-of-jails — the same check the MCP transport's
            // tole_session_new applies: without it a client could root a
            // session (and its file/git tools) at `/` or `/etc`.
            let jailed = match tole_cli::session_tools::jail_session_cwd(
                &state.workspace_root,
                &requested,
            ) {
                Ok(c) => c,
                Err(e) => return (403, json!({"error": e})),
            };
            let cwd = jailed.as_str();
            // Capacity first: `open_session` creates the session file on
            // disk, so refusing AFTER it (503) would orphan that file. The
            // re-check at insert time below still guards a concurrent
            // creator.
            if lock_sessions(&state.sessions).ensure_room().is_err() {
                return (503, json!({"error": "all sessions busy — at capacity"}));
            }
            let session_id = tole_cli::session_host::new_session_id("srv");
            // Issue #200: non-preauthorized Writes queue for a remote
            // decision (deny now, resumable; allow = one-shot replay).
            // Queue-time storage path (cora MAJOR): an evicted session's
            // audit fallback must reopen the EXACT durable file — with
            // no --sessions-dir the log lives under the session's cwd.
            let storage_dir: std::path::PathBuf = match &state.sessions_dir {
                Some(d) => d.clone(),
                None => std::path::Path::new(cwd).join(".tole/sessions"),
            };
            let approver = tole_cli::approvals::QueueApprover::new(
                state.allow_patterns.clone(),
                Arc::clone(&state.approvals),
                &session_id,
                storage_dir.join(format!("{session_id}.jsonl")),
            );
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
                // No plan UI on the REST face.
                None,
            ) {
                Ok(session_state) => {
                    // Cap the live map (the registry's admission rule): a
                    // long-running daemon must not grow without bound
                    // (CodeCora scan-3). Eviction drops only the in-memory
                    // handle, never the file; busy sessions are never
                    // evicted, and all-busy at capacity is a 503.
                    if lock_sessions(&state.sessions)
                        .admit(session_id.clone(), session_state)
                        .is_err()
                    {
                        return (503, json!({"error": "all sessions busy — at capacity"}));
                    }
                    (200, json!({"sessionId": session_id, "workspace": cwd}))
                }
                Err(e) => (500, json!({"error": e})),
            }
        }
        // Remote approvals (issue #200): list + decide.
        ("GET", p) if p == "/approvals" || p.starts_with("/approvals?") => {
            let session = p.split_once('?').and_then(|(_, q)| {
                q.split('&')
                    .find_map(|kv| kv.strip_prefix("session=").map(str::to_string))
            });
            let list = state
                .approvals
                .list(session.as_deref())
                .iter()
                .map(tole_cli::approvals::approval_json)
                .collect::<Vec<_>>();
            (200, json!({"approvals": list}))
        }
        (method, p)
            if method == "POST" && (p.starts_with("/approvals/") && p.ends_with("/decision")) =>
        {
            let id = p
                .trim_start_matches("/approvals/")
                .trim_end_matches("/decision")
                .to_string();
            let Ok(req) = serde_json::from_str::<serde_json::Value>(body) else {
                return (400, json!({"error": "invalid JSON body"}));
            };
            let allow = match req.get("decision").and_then(|v| v.as_str()) {
                Some("allow") => true,
                Some("deny") => false,
                _ => return (400, json!({"error": "decision must be 'allow' or 'deny'"})),
            };
            let (session_id, tool, status) = match state.approvals.decide(&id, allow) {
                tole_cli::approvals::DecisionOutcome::Decided {
                    status,
                    session_id,
                    tool,
                } => (session_id, tool, status),
                tole_cli::approvals::DecisionOutcome::Unknown => {
                    return (404, json!({"error": "unknown approval id"}));
                }
            };
            // Durable audit — REQUIRED (cora MAJOR): the decision lands
            // on the session even when the in-memory handle was evicted
            // (reopen from disk) and even when a turn holds the storage
            // lock (block: the audit contract outweighs latency here).
            // A total failure is a 5xx, never a silent skip.
            let audit = write_approval_audit(state, &session_id, &id, allow, &tool);
            if audit.is_err() {
                return (
                    500,
                    json!({"error": "audit write failed — decision NOT recorded;                            the queue entry reflects the verdict but the durable                            trail is incomplete"}),
                );
            }
            if allow {
                // Approvals-only resume: the replayed guarded effect
                // consults the approver, the one-shot matches, the tool
                // executes. Runs off-thread — the turn may take a while.
                // Bounded without the connection cap: an approval id is decided
                // once (a repeat is 404 above) and pending approvals only come
                // from the capped, one-turn-per-session map.
                let sessions = Arc::clone(&state.sessions);
                let sid = session_id.clone();
                std::thread::spawn(move || {
                    if let Err(e) = tole_cli::session_host::resume_session_turn(sessions, &sid) {
                        eprintln!("tole serve: approval resume failed: {e}");
                    }
                });
                (
                    202,
                    json!({"id": id, "status": status.as_str(), "resuming": true}),
                )
            } else {
                (200, json!({"id": id, "status": status.as_str()}))
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
                    let (busy, entries, report) = {
                        let sessions = lock_sessions(&state.sessions);
                        let Some(st) = sessions.get(id) else {
                            return (404, json!({"error": "unknown session"}));
                        };
                        let busy = *st.busy.lock().expect("busy lock");
                        let (entries, report) = match st.storage.try_lock() {
                            Ok(guard) => (
                                Some(guard.entries().len()),
                                // Issue #211: additive usage report.
                                tole_cli::usage_report::usage_json(guard.entries(), guard.usages()),
                            ),
                            // turn in flight — skip the count and report
                            Err(_) => (None, json!(null)),
                        };
                        (busy, entries, report)
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
                            "usage_report": report,
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
                        sessions.cancel(id)
                    };
                    if cancelled {
                        eprintln!("tole: session/{id} cancel requested (serve)");
                        (200, json!({"cancelled": true, "sessionId": id}))
                    } else {
                        (404, json!({"error": "unknown session"}))
                    }
                }
                ("POST", id, Some("interrupt")) => {
                    // Soft interrupt (#377): steer the in-flight turn
                    // without cancelling it. 409 when no turn is running.
                    let Ok(req) = serde_json::from_str::<serde_json::Value>(body) else {
                        return (400, json!({"error": "invalid JSON body"}));
                    };
                    let Some(text) = req.get("text").and_then(|v| v.as_str()) else {
                        return (400, json!({"error": "missing text"}));
                    };
                    let urgent = req.get("urgent").and_then(|v| v.as_bool()).unwrap_or(false);
                    let st = {
                        let sessions = lock_sessions(&state.sessions);
                        sessions.get(id).cloned()
                    };
                    let Some(st) = st else {
                        return (404, json!({"error": "unknown session"}));
                    };
                    match tole_cli::session_host::interrupt_session(&st, text, urgent) {
                        Ok(()) => {
                            eprintln!(
                                "tole: session/{id} interrupt queued (serve, urgent={urgent})"
                            );
                            (
                                202,
                                json!({"queued": true, "urgent": urgent, "sessionId": id}),
                            )
                        }
                        Err(tole_cli::session_host::InterruptRefusal::NotBusy) => {
                            (409, json!({"error": "no turn is running in this session"}))
                        }
                        Err(tole_cli::session_host::InterruptRefusal::Invalid(e)) => {
                            (400, json!({"error": e}))
                        }
                    }
                }
                _ => (404, json!({"error": "not found"})),
            }
        }
        _ => (404, json!({"error": "not found"})),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reason_phrases_cover_every_status_the_routes_emit() {
        assert_eq!(reason_phrase(202), "Accepted");
        assert_eq!(reason_phrase(429), "Too Many Requests");
        assert_eq!(reason_phrase(503), "Service Unavailable");
        assert_eq!(reason_phrase(413), "Payload Too Large");
        assert_eq!(reason_phrase(403), "Forbidden");
    }

    fn test_state(root: std::path::PathBuf, sessions_dir: std::path::PathBuf) -> State {
        State {
            sessions: Arc::new(Mutex::new(Sessions::default())),
            approvals: tole_cli::approvals::ApprovalQueue::shared(),
            allow_patterns: vec![],
            plan_mode: false,
            memory: None,
            token: "t".into(),
            workspace_root: root,
            sessions_dir: Some(sessions_dir),
            turnend: vec![],
            live_connections: std::sync::atomic::AtomicUsize::new(0),
            auth_failures: tole_cli::auth::AuthLimiter::new(),
        }
    }

    /// cora 0.19 scan MAJOR: REST `POST /sessions` must enforce the same
    /// jail-of-jails as the MCP transport — a cwd outside the server
    /// workspace root is refused BEFORE any session file is created.
    #[test]
    fn rest_session_cwd_outside_workspace_root_is_refused() {
        let base = std::env::temp_dir().join(format!("tole-serve-jail-{}", std::process::id()));
        let root = base.join("ws");
        let inside = root.join("sub");
        let sessions = base.join("sessions");
        std::fs::create_dir_all(&inside).unwrap();
        std::fs::create_dir_all(&sessions).unwrap();
        let state = test_state(root.clone(), sessions.clone());

        for escape in [
            "/",
            base.to_str().unwrap(),
            &format!("{}/..", root.display()),
        ] {
            let body = json!({ "cwd": escape }).to_string();
            let (status, payload) = route(&state, "POST", "/sessions", &body);
            assert_eq!(status, 403, "{escape}: {payload}");
            assert!(payload["error"].as_str().unwrap().contains("escapes"));
        }
        assert_eq!(
            std::fs::read_dir(&sessions).unwrap().count(),
            0,
            "a refused cwd must not leave a session file behind"
        );

        // Inside the root (and the default, omitted cwd) still open.
        let body = json!({ "cwd": inside.to_str().unwrap() }).to_string();
        let (status, payload) = route(&state, "POST", "/sessions", &body);
        assert_eq!(status, 200, "{payload}");
        assert_eq!(
            payload["workspace"],
            json!(inside.canonicalize().unwrap().to_str().unwrap())
        );
        let (status, payload) = route(&state, "POST", "/sessions", "{}");
        assert_eq!(status, 200, "{payload}");
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn deadline_reader_bounds_a_trickling_client() {
        // A client sending one byte per 100 ms never trips the per-read
        // timeout; the total deadline must still cut the read off.
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let writer = std::thread::spawn(move || {
            let mut c = TcpStream::connect(addr).unwrap();
            for _ in 0..40 {
                if c.write_all(b"x").is_err() {
                    return;
                }
                std::thread::sleep(Duration::from_millis(100));
            }
        });
        let (server_side, _) = listener.accept().unwrap();
        let mut r = DeadlineReader {
            stream: &server_side,
            deadline: Instant::now() + Duration::from_millis(400),
        };
        let started = Instant::now();
        let mut buf = [0u8; 1];
        let err = loop {
            match r.read(&mut buf) {
                Ok(0) => panic!("peer closed before the deadline"),
                Ok(_) => continue,
                Err(e) => break e,
            }
        };
        assert!(matches!(
            err.kind(),
            std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock
        ));
        assert!(started.elapsed() < Duration::from_secs(2));
        drop(server_side);
        let _ = writer.join();
    }
}
