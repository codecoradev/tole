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
//! - `POST /sessions` also takes `{"resume": "<id>"}` to reopen an existing
//!   session (#524)
//! - `GET  /sessions/{id}/events?after=<seq>` → Server-Sent Events: durable
//!   `entry` frames replayed from a `seq`, ephemeral text/tool/approval
//!   frames (#524; see `tole_cli::events`)
//! - `GET  /sessions/{id}/entries?after=<seq>&limit=<n>` → one page of the
//!   session log (#524; see `tole_cli::protocol`)
//! - `POST /sessions/{id}/interrupt` → steer the running turn without
//!   cancelling it (`{"text": "...", "urgent": false}`, #377)
//! - `POST /sessions/{id}/images` → upload one image as the raw body (#558);
//!   `POST /sessions/{id}/prompt` takes `"images": [<id>]` next to `text`
//! - `GET  /sessions/{id}/images/{fnv1a64}` → the bytes of an image recorded
//!   in that session's log (#558; see `tole_cli::serve_images`)
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
    lock_sessions, open_session, run_session_turn_with_images, Sessions, SharedSessions,
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
    /// Per-session ephemeral event buses for `GET /sessions/{id}/events`.
    buses: tole_cli::events::BusRegistry,
    /// Caps open event streams apart from (and within) the connection cap.
    streams: Arc<tole_cli::events::StreamLimiter>,
    /// Per-source-IP failed-auth limiter (brute-force hardening), shared
    /// logic with the MCP transport.
    auth_failures: tole_cli::auth::AuthLimiter,
    /// Image uploads (#558); `None` when no data directory resolves.
    uploads: Option<tole_cli::serve_images::UploadStore>,
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
/// Body budget of an authenticated image upload (#558): up to 10 MiB from a
/// phone link needs more than [`REQUEST_READ_DEADLINE`]. Only granted once the
/// headers carried the right token.
const UPLOAD_READ_DEADLINE: Duration = Duration::from_secs(120);

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
        buses: tole_cli::events::BusRegistry::default(),
        streams: tole_cli::events::StreamLimiter::new(tole_cli::events::MAX_STREAMS),
        auth_failures: tole_cli::auth::AuthLimiter::new(),
        uploads: tole_cli::serve_images::UploadStore::from_env(),
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
    /// `Last-Event-ID`: where an SSE client left off (`EventSource` sends it
    /// on reconnect).
    last_event_id: Option<String>,
    /// Raw body bytes (an image upload is binary; JSON routes decode it).
    body: Vec<u8>,
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
/// Ceiling for an authenticated image upload body (#558): the core's
/// per-image cap. Unauthenticated requests keep [`MAX_BODY_BYTES`].
const MAX_UPLOAD_BYTES: usize = tole_core::image::MAX_IMAGE_BYTES as usize;

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

fn read_request(stream: &TcpStream, token: &str) -> Result<Request, ReadError> {
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
    let mut last_event_id = None;
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
        } else if name.eq_ignore_ascii_case("last-event-id") {
            last_event_id = Some(value.to_string());
        } else if name.eq_ignore_ascii_case("expect") && value.eq_ignore_ascii_case("100-continue")
        {
            expect_continue = true;
        }
    }
    let content_length = content_length.unwrap_or(0);
    // The larger upload ceiling only for an image upload that already
    // carries the right token: pre-auth buffering stays at MAX_BODY_BYTES.
    let upload = method == "POST"
        && upload_target(&path).is_some()
        && tole_cli::auth::bearer_matches(authorization.as_deref(), token);
    let ceiling = if upload {
        MAX_UPLOAD_BYTES
    } else {
        MAX_BODY_BYTES
    };
    if content_length > ceiling {
        return Err(ReadError::TooLarge); // refuse rather than buffer it
    }
    if upload {
        reader.get_mut().deadline = Instant::now() + UPLOAD_READ_DEADLINE;
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
        last_event_id,
        body,
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
        415 => "Unsupported Media Type",
        429 => "Too Many Requests",
        500 => "Internal Server Error",
        503 => "Service Unavailable",
        507 => "Insufficient Storage",
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
    let req = match read_request(&stream, &state.token) {
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

    // The event stream owns its connection for as long as it is open, so it
    // is served here rather than through `route` (which returns one JSON
    // body).
    if req.method == "GET" {
        if let Some((id, query)) = events_target(&req.path) {
            serve_events(stream, &state, id, query, req.last_event_id.as_deref());
            return;
        }
    }

    // Images (#558): a binary body in, binary bytes out.
    if req.method == "POST" {
        if let Some(id) = upload_target(&req.path) {
            let (status, payload) = upload_image(&state, id, &req.body);
            respond(&mut stream, status, &payload);
            return;
        }
    }
    if req.method == "GET" {
        if let Some((id, hash)) = image_target(&req.path) {
            match read_image(&state, id, hash) {
                Ok((mime, data)) => respond_image(&mut stream, &mime, &data),
                Err((status, payload)) => respond(&mut stream, status, &payload),
            }
            return;
        }
    }

    let body = String::from_utf8_lossy(&req.body);
    let (status, payload) = route(&state, &req.method, &req.path, &body);
    respond(&mut stream, status, &payload);
}

/// `/sessions/{id}/images` -> `id` (the upload route).
fn upload_target(path: &str) -> Option<&str> {
    let id = path.strip_prefix("/sessions/")?.strip_suffix("/images")?;
    (!id.is_empty() && !id.contains('/') && !id.contains('?')).then_some(id)
}

/// `/sessions/{id}/images/{hash}` -> `(id, hash)` (the read route).
fn image_target(path: &str) -> Option<(&str, &str)> {
    let rest = path.strip_prefix("/sessions/")?;
    let (id, hash) = rest.split_once("/images/")?;
    let ok = |s: &str| !s.is_empty() && !s.contains('/') && !s.contains('?');
    (ok(id) && ok(hash)).then_some((id, hash))
}

fn image_error(e: tole_cli::serve_images::ImageApiError) -> (u16, serde_json::Value) {
    (e.status(), json!({"error": e.message()}))
}

/// The JSONL file of a live session, or the 404 answer.
fn live_session_path(
    state: &State,
    id: &str,
) -> Result<std::path::PathBuf, (u16, serde_json::Value)> {
    let sessions = lock_sessions(&state.sessions);
    sessions
        .get(id)
        .map(|st| st.path.clone())
        .ok_or_else(|| (404, json!({"error": "unknown session"})))
}

/// `POST /sessions/{id}/images` (#558): store the raw body as an image of
/// a live session; answers the reference (`id`, `mime`, `bytes`, `fnv1a64`).
fn upload_image(state: &State, id: &str, body: &[u8]) -> (u16, serde_json::Value) {
    if let Err(resp) = live_session_path(state, id) {
        return resp;
    }
    let Some(store) = state.uploads.as_ref() else {
        return (
            503,
            json!({"error": "image uploads need a data directory (set CODECORA_HOME or HOME)"}),
        );
    };
    match store.store(id, body) {
        Ok(r) => (200, tole_cli::serve_images::upload_json(&r)),
        Err(e) => image_error(e),
    }
}

/// `GET /sessions/{id}/images/{fnv1a64}` (#558): only an image the session's
/// log records, re-checked against the jail, its size and its hash.
fn read_image(
    state: &State,
    id: &str,
    hash: &str,
) -> Result<(String, Vec<u8>), (u16, serde_json::Value)> {
    let path = live_session_path(state, id)?;
    tole_cli::serve_images::read_recorded_in_jail(&path, hash).map_err(image_error)
}

/// An image body with a fixed type: the recorded mime, never sniffed by
/// the client (`nosniff`), never cached by a shared cache.
fn respond_image(stream: &mut TcpStream, mime: &str, data: &[u8]) {
    let head = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: {mime}\r\nContent-Length: {}\r\nX-Content-Type-Options: nosniff\r\nCache-Control: private, no-store\r\nConnection: close\r\n\r\n",
        data.len()
    );
    let _ = stream.write_all(head.as_bytes());
    let _ = stream.write_all(data);
    let _ = stream.flush();
}

/// `/sessions/{id}/events[?query]` -> `(id, query)`.
fn events_target(path: &str) -> Option<(&str, &str)> {
    let (path, query) = path.split_once('?').unwrap_or((path, ""));
    let id = path.strip_prefix("/sessions/")?.strip_suffix("/events")?;
    (!id.is_empty() && !id.contains('/')).then_some((id, query))
}

fn write_all(stream: &mut TcpStream, text: &str) -> bool {
    stream
        .write_all(text.as_bytes())
        .and_then(|()| stream.flush())
        .is_ok()
}

/// `GET /sessions/{id}/events?after=<seq>` (#524): Server-Sent Events.
///
/// Durable `entry` frames replay from `after` (or `Last-Event-ID`) and then
/// follow the log; ephemeral frames (`text_delta`, `reasoning`,
/// `tool_started`, `tool_finished`, `approval_*`) are best-effort and not
/// replayable. See `tole_cli::events` for the model. The stream ends when
/// the client goes away (a write fails, or the heartbeat finds it dead) or
/// the session leaves the live map (`closed`).
fn serve_events(
    mut stream: TcpStream,
    state: &Arc<State>,
    id: &str,
    query: &str,
    last_event_id: Option<&str>,
) {
    use tole_cli::events::{entry_frame, frame, ApprovalWatch, EntryTail, Heartbeat, HEARTBEAT};
    let param = |name: &str| {
        query
            .split('&')
            .find_map(|kv| kv.strip_prefix(name).and_then(|v| v.strip_prefix('=')))
    };
    // An explicit `after` wins over the header; both are a `seq`.
    let after = match param("after") {
        Some(a) => match a.parse::<u64>() {
            Ok(n) => n,
            Err(_) => {
                respond(
                    &mut stream,
                    400,
                    &json!({"error": "after must be a non-negative integer"}),
                );
                return;
            }
        },
        None => last_event_id
            .and_then(|v| v.trim().parse::<u64>().ok())
            .unwrap_or(0),
    };
    let path = {
        let sessions = lock_sessions(&state.sessions);
        sessions.get(id).map(|st| st.path.clone())
    };
    let Some(path) = path else {
        respond(&mut stream, 404, &json!({"error": "unknown session"}));
        return;
    };
    let Some(_slot) = state.streams.try_acquire() else {
        respond(
            &mut stream,
            429,
            &json!({"error": "too many open event streams"}),
        );
        return;
    };
    // Subscribe BEFORE replaying so no ephemeral frame falls in the gap.
    let sub = state.buses.subscribe(id);
    let head = "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nCache-Control: no-cache\r\nX-Accel-Buffering: no\r\nConnection: close\r\n\r\n";
    if !write_all(&mut stream, head) {
        return;
    }

    let live_busy = |state: &State| -> Option<bool> {
        let sessions = lock_sessions(&state.sessions);
        sessions
            .get(id)
            .map(|st| *st.busy.lock().expect("busy lock"))
    };
    let mut tail = EntryTail::new(path, after);
    let mut approvals = ApprovalWatch::default();
    let mut ready_sent = false;
    let mut hb = Heartbeat::new(
        tole_cli::events::heartbeat_interval(
            std::env::var(tole_cli::events::HEARTBEAT_ENV)
                .ok()
                .as_deref(),
        ),
        Instant::now(),
    );
    // Approval state changes go out BEFORE the durable entries of the same
    // pass, so a decision is announced ahead of the effects it caused.
    // `Some(n)`: n frames written (0 is normal); `None`: the peer is gone.
    let send_approvals = |stream: &mut TcpStream, approvals: &mut ApprovalWatch| -> Option<usize> {
        let mut n = 0;
        for (name, data) in approvals.poll(&state.approvals.list(Some(id))) {
            if !write_all(stream, &frame(name, None, &data)) {
                return None;
            }
            n += 1;
        }
        Some(n)
    };
    loop {
        if ready_sent {
            match send_approvals(&mut stream, &mut approvals) {
                None => return,
                Some(0) => {}
                Some(_) => hb.note_write(Instant::now()),
            }
        }
        // Durable frames (a transient read error just retries).
        if let Ok(entries) = tail.poll() {
            for e in entries {
                if !write_all(&mut stream, &entry_frame(&e)) {
                    return;
                }
                hb.note_write(Instant::now());
            }
        }
        if !ready_sent {
            ready_sent = true;
            let ready = frame(
                "ready",
                None,
                &json!({"id": id, "after": after, "lastSeq": tail.last_seq(),
                        "busy": live_busy(state).unwrap_or(false)}),
            );
            // The replay is complete; then what is pending right now.
            if !write_all(&mut stream, &ready) {
                return;
            }
            hb.note_write(Instant::now());
            if send_approvals(&mut stream, &mut approvals).is_none() {
                return;
            }
        }
        let dropped = sub.take_dropped();
        if dropped > 0 {
            if !write_all(
                &mut stream,
                &frame("resync", None, &json!({"dropped": dropped})),
            ) {
                return;
            }
            hb.note_write(Instant::now());
        }
        // Ephemeral frames: wait briefly for one (this is also the poll
        // interval for the durable tail), then drain what queued up.
        match sub.rx.recv_timeout(Duration::from_millis(150)) {
            Ok(first) => {
                let mut next = Some(first);
                while let Some(e) = next {
                    if !write_all(&mut stream, &e.to_frame()) {
                        return;
                    }
                    hb.note_write(Instant::now());
                    next = sub.rx.try_recv().ok();
                }
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => return,
        }
        if live_busy(state).is_none() {
            // Evicted from the live map: say so and end cleanly. Entries
            // written before the end have been sent (one last poll above).
            let _ = tail
                .poll()
                .map(|rest| rest.iter().all(|e| write_all(&mut stream, &entry_frame(e))));
            let _ = write_all(&mut stream, &frame("closed", None, &json!({"id": id})));
            return;
        }
        // Silence for HEARTBEAT_SECS: write a comment. This is also how a
        // dead peer is found (the write fails) and its slot freed.
        if hb.due(Instant::now()) {
            if !write_all(&mut stream, HEARTBEAT) {
                return;
            }
            hb.note_write(Instant::now());
        }
    }
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

/// Open (or reopen, `loading`) a serve session and admit it to the live map.
/// Shared by create and resume so both get the same approver, jail and
/// capacity rules. `Err` is the HTTP answer.
fn open_serve_session(
    state: &State,
    session_id: &str,
    cwd: &str,
    loading: bool,
) -> Result<(), (u16, serde_json::Value)> {
    // Issue #200: non-preauthorized Writes queue for a remote
    // decision (deny now, resumable; allow = one-shot replay).
    // Queue-time storage path (cora MAJOR): an evicted session's
    // audit fallback must reopen the EXACT durable file — with
    // no --sessions-dir the log lives under the session's cwd.
    let storage_dir = serve_storage_dir(state, cwd);
    let approver = tole_cli::approvals::QueueApprover::new(
        state.allow_patterns.clone(),
        Arc::clone(&state.approvals),
        session_id,
        storage_dir.join(format!("{session_id}.jsonl")),
    );
    match open_session(
        session_id,
        cwd,
        loading,
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
                .admit(session_id.to_string(), session_state)
                .is_err()
            {
                return Err((503, json!({"error": "all sessions busy — at capacity"})));
            }
            Ok(())
        }
        // Another process (a TUI, a second serve) holds the writer lock.
        Err(e) if e.contains("open for writing") => Err((409, json!({"error": e}))),
        Err(e) => Err((500, json!({"error": e}))),
    }
}

/// Where this serve process keeps the session files of `cwd`.
fn serve_storage_dir(state: &State, cwd: &str) -> std::path::PathBuf {
    match &state.sessions_dir {
        Some(d) => d.clone(),
        None => std::path::Path::new(cwd).join(".tole/sessions"),
    }
}

/// `POST /sessions {"resume": "<id>"}` (#524): make an existing session live
/// again. `cwd` has already passed the jail check, and the id is validated
/// and only ever joined into the session directory (no client-supplied path),
/// so a client cannot open a file outside the jail.
///
/// - an already-live session is returned as is (200, `alreadyLive`): nothing
///   is reopened or replaced, so a busy turn is never disturbed, and a client
///   that reconnects mid-turn gets its id back plus the `busy` flag;
/// - no such file in that directory is a 404;
/// - another process holding the session's writer lock is a 409.
fn resume_session(state: &State, id: &str, cwd: &str) -> (u16, serde_json::Value) {
    if tole_cli::session_host::validate_session_id(id).is_none() {
        return (400, json!({"error": "invalid session id"}));
    }
    // Live first: the writer lock is held by this very process, so a second
    // open of the same file would be refused as "locked".
    {
        let sessions = lock_sessions(&state.sessions);
        if let Some(st) = sessions.get(id) {
            let busy = *st.busy.lock().expect("busy lock");
            return (
                200,
                json!({"sessionId": id, "resumed": false, "alreadyLive": true, "busy": busy}),
            );
        }
    }
    let file = serve_storage_dir(state, cwd).join(format!("{id}.jsonl"));
    if !file.is_file() {
        return (404, json!({"error": "unknown session"}));
    }
    if lock_sessions(&state.sessions).ensure_room().is_err() {
        return (503, json!({"error": "all sessions busy — at capacity"}));
    }
    match open_serve_session(state, id, cwd, true) {
        Ok(()) => (
            200,
            json!({"sessionId": id, "workspace": cwd, "resumed": true, "alreadyLive": false}),
        ),
        Err(resp) => resp,
    }
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
            // Resume-by-id (#524): reopen an existing session instead of
            // creating one. Same jail, same capacity and approval rules.
            match req.get("resume") {
                None | Some(serde_json::Value::Null) => {}
                Some(serde_json::Value::String(id)) => return resume_session(state, id, cwd),
                Some(_) => return (400, json!({"error": "resume must be a session id string"})),
            }
            // Capacity first: `open_session` creates the session file on
            // disk, so refusing AFTER it (503) would orphan that file. The
            // re-check at insert time below still guards a concurrent
            // creator.
            if lock_sessions(&state.sessions).ensure_room().is_err() {
                return (503, json!({"error": "all sessions busy — at capacity"}));
            }
            let session_id = tole_cli::session_host::new_session_id("srv");
            match open_serve_session(state, &session_id, cwd, false) {
                Ok(()) => (200, json!({"sessionId": session_id, "workspace": cwd})),
                Err(resp) => resp,
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
            // The query string belongs to the route, not to the action
            // name (`entries?after=5` is the `entries` action).
            let (path, query) = path.split_once('?').unwrap_or((path, ""));
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
                ("GET", id, Some("entries")) => {
                    // Paginated transcript read (#524). Replays the
                    // session's FILE, so it works while a turn runs.
                    let param = |name: &str| {
                        query
                            .split('&')
                            .find_map(|kv| kv.strip_prefix(name).and_then(|v| v.strip_prefix('=')))
                    };
                    let (after, limit) = match tole_cli::protocol::parse_entries_query(
                        param("after"),
                        param("limit"),
                    ) {
                        Ok(v) => v,
                        Err(tole_cli::protocol::EntriesError::BadQuery(m)) => {
                            return (400, json!({"error": m}))
                        }
                        Err(tole_cli::protocol::EntriesError::Read(m)) => {
                            return (500, json!({"error": m}))
                        }
                    };
                    let path = {
                        let sessions = lock_sessions(&state.sessions);
                        sessions.get(id).map(|st| st.path.clone())
                    };
                    let Some(path) = path else {
                        return (404, json!({"error": "unknown session"}));
                    };
                    match tole_cli::protocol::entries_page(&path, id, after, limit) {
                        Ok(page) => (200, serde_json::to_value(&page).unwrap_or(json!(null))),
                        Err(tole_cli::protocol::EntriesError::Read(m)) => {
                            (500, json!({"error": m}))
                        }
                        Err(tole_cli::protocol::EntriesError::BadQuery(m)) => {
                            (400, json!({"error": m}))
                        }
                    }
                }
                ("POST", id, Some("prompt")) => {
                    let Ok(req) = serde_json::from_str::<serde_json::Value>(body) else {
                        return (400, json!({"error": "invalid JSON body"}));
                    };
                    let Some(text) = req.get("text").and_then(|v| v.as_str()) else {
                        return (400, json!({"error": "missing text"}));
                    };
                    // Images (#558): ids of this session's uploads, resolved
                    // to durable references before the turn starts.
                    let images = match req.get("images") {
                        None | Some(serde_json::Value::Null) => vec![],
                        Some(v) => {
                            if lock_sessions(&state.sessions).get(id).is_none() {
                                return (404, json!({"error": "unknown session"}));
                            }
                            let Some(store) = state.uploads.as_ref() else {
                                return (
                                    503,
                                    json!({"error": "image uploads need a data directory"}),
                                );
                            };
                            match store.lookup_all(id, Some(v)) {
                                Ok(refs) => refs,
                                Err(e) => return image_error(e),
                            }
                        }
                    };
                    // An observer (text deltas, tool events) only while an event
                    // stream is open on this session: with nobody watching the
                    // turn runs exactly as before streams existed.
                    let observer = state.buses.observer_for(id);
                    match run_session_turn_with_images(
                        Arc::clone(&state.sessions),
                        id,
                        text,
                        &images,
                        observer,
                    ) {
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
                ("POST", id, Some("detach")) => {
                    // Detach (#217): stop waiting without aborting; the turn
                    // ends "detached". 409 when no turn is running.
                    let st = {
                        let sessions = lock_sessions(&state.sessions);
                        sessions.get(id).cloned()
                    };
                    let Some(st) = st else {
                        return (404, json!({"error": "unknown session"}));
                    };
                    if tole_cli::session_host::detach_session(&st) {
                        eprintln!("tole: session/{id} detach requested (serve)");
                        (202, json!({"detached": true, "sessionId": id}))
                    } else {
                        (409, json!({"error": "no turn is running in this session"}))
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
            buses: tole_cli::events::BusRegistry::default(),
            streams: tole_cli::events::StreamLimiter::new(tole_cli::events::MAX_STREAMS),
            auth_failures: tole_cli::auth::AuthLimiter::new(),
            uploads: None,
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
