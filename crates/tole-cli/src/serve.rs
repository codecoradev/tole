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
//! `--socket <path>` (#1043) listens on a Unix domain socket in a private (0700)
//! directory instead of TCP; routes, auth and the event stream are the same
//! code path (see `tole_cli::serve_support::transport`). `GET /health` also
//! returns the protocol and binary version.
//!
//! v1 scope: REST only (MCP-over-HTTP is the follow-up; the engine calls
//! are identical). Auth is a static bearer token (env `TOLE_SERVE_TOKEN`,
//! `--token-file` or `--token`) or, with `--auth basic`, HTTP Basic against a
//! password hash (#742; the token still works next to it when one is set) —
//! refuses to start without credentials. Binds 127.0.0.1 by default; `--bind`
//! overrides for trusted networks (Basic refuses a non-loopback bind unless
//! `--allow-insecure-basic`: TLS terminates at a reverse proxy).
//!
//! HTTP is hand-rolled on std::net (tole's no-new-deps rule for host
//! plumbing): HTTP/1.1, Content-Length bodies only, one request per
//! connection (`Connection: close`). Runs turn-by-turn synchronously per
//! request; concurrent prompts on the same session get 409 (busy).

use anyhow::{Context, Result};
use serde_json::json;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tole_cli::serve_support::profiles::{self, ProfileSource, SessionProfile};
use tole_cli::serve_support::transport::{self, Conn, Listener};
use tole_cli::session_host::{
    lock_sessions, open_session_with, run_session_turn_with_end, SessionOverrides, Sessions,
    SharedSessions,
};
use tole_core::memory::MemoryConfig;
use tole_core::storage::Storage;

pub mod missions;
mod models;
pub mod past_sessions;

pub struct ServeConfig {
    pub bind: String,
    /// `--socket` (#1043): listen on this Unix socket instead of TCP.
    pub socket: Option<std::path::PathBuf>,
    pub port: u16,
    /// The one authentication check (#742): bearer token and/or Basic.
    pub auth: tole_cli::auth::Authenticator,
    pub allow_patterns: Vec<String>,
    pub workspace: Option<String>,
    pub plan_mode: bool,
    pub memory: Option<MemoryConfig>,
    /// Explicit `--sessions-dir` override (None = per-session-cwd
    /// default, the pre-existing behavior).
    pub sessions_dir: Option<std::path::PathBuf>,
    /// `--on-turnend` stop gates wired onto every session registry.
    pub turnend: Vec<String>,
    /// `--cron`: host the cron ticker in-process (#579).
    pub cron: bool,
    /// Where per-session profiles come from and what the command line
    /// permits (#740).
    pub profiles: ProfileSource,
    /// What the owner configured for `POST /missions` (#947): the verify
    /// command, its timeout and the budget ceilings. Never a request field.
    pub mission: missions::MissionSettings,
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
    /// Per-session profiles (#740): user config only.
    profiles: ProfileSource,
    plan_mode: bool,
    memory: Option<MemoryConfig>,
    /// Every route's auth, through `Authenticator::check` only (#742).
    auth: tole_cli::auth::Authenticator,
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
    /// Webhook routes (#582) from the user config.
    hooks: std::collections::BTreeMap<String, tole_cli::hooks::Route>,
    /// The cron dir, whose ledger records seen deliveries (`hook_seen`).
    hook_dir: Option<std::path::PathBuf>,
    /// Approvals whose first decision took effect in the queue but whose
    /// durable audit write failed (the client got a 5xx): a retry of the same
    /// verdict redoes the audit and the resume instead of being told it is a
    /// repeat.
    audit_pending: Mutex<std::collections::BTreeSet<String>>,
    /// Owner configuration of `POST /missions` (#947).
    mission: missions::MissionSettings,
    /// The missions this process started (one runs at a time).
    missions: Arc<missions::Missions>,
    /// What a client may pick for `GET /models` (#574): the owner's list.
    models: models::ModelCatalog,
    /// The `turn_finished` webhook callback (#962), set once at start-up when a
    /// destination lists the event. Empty, no turn path does anything extra.
    turn_events: std::sync::OnceLock<tole_cli::webhook_out::turn::TurnNotifier>,
}

/// Announces a settled turn of a `tole serve` session to the destinations that
/// list `turn_finished` (#962, provisional D5: serve sessions only). `end` is the
/// typed end built in the session host, with no text in it. The approval id of an
/// `approval_pending` end is read from the queue entry this turn queued, never
/// from the outcome. Never blocks beyond a queue push and never fails the turn.
fn announce_turn(
    notifier: Option<&tole_cli::webhook_out::turn::TurnNotifier>,
    approvals: &tole_cli::approvals::ApprovalQueue,
    session_id: &str,
    end: Option<tole_cli::webhook_out::turn::TurnEnd>,
    approval_tool: Option<&str>,
    queue_mark: Option<u64>,
) {
    let (Some(notify), Some(end)) = (notifier, end) else {
        return;
    };
    let approval_id = approval_tool
        .zip(queue_mark)
        .and_then(|(tool, mark)| approvals.queued_since(session_id, tool, mark))
        .filter(|e| e.status == tole_cli::approvals::ApprovalStatus::Pending)
        .map(|e| e.id);
    let signal = tole_cli::webhook_out::turn::TurnSignal {
        session_id: session_id.to_string(),
        end,
        approval_id,
    };
    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| notify(signal)));
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
    // No human answers a serve session's start-up: an `optional` sandbox
    // counts as `required` here (#601).
    tole_core::sandbox::mark_unattended();
    let workspace_root = crate::resolve_workspace_root(cfg.workspace.as_ref())
        .context("resolving --workspace for the session jail root")?;
    // Webhook routes (#582), user config only: a route that cannot load
    // stops the server here, before it listens.
    let hook_cfg = match tole_cli::config::user_config_path() {
        Some(p)
            if tole_cli::config::user_config_present(&p).map_err(|e| anyhow::anyhow!("{e}"))? =>
        {
            tole_cli::config::load(&p)
                .map_err(|e| anyhow::anyhow!("{e}"))?
                .hooks
                .unwrap_or_default()
        }
        _ => Default::default(),
    };
    // A local socket is reachable by the owner's processes only.
    let loopback = cfg.socket.is_some() || tole_cli::auth::is_loopback_bind(&cfg.bind);
    let hooks = tole_cli::hooks::load(&hook_cfg, loopback, &|k| std::env::var(k).ok())
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    tole_cli::hooks::hold_keys(&hooks);
    // Outbound webhook destinations (#911), user config only: one that cannot
    // load stops the server here too, with its name and the fix. A running
    // server keeps this map until restart.
    let webhook_out = tole_cli::webhook_out::load_user().map_err(|e| anyhow::anyhow!("{e}"))?;
    let webhook_out = tole_cli::webhook_out::install(webhook_out);
    let hook_dir = if hooks.is_empty() {
        None
    } else {
        Some(tole_cli::cron::default_dir()?)
    };
    let (listener, shown) = match &cfg.socket {
        Some(path) => (
            transport::bind_socket(path).map_err(|e| anyhow::anyhow!("{e}"))?,
            format!("unix:{}", path.display()),
        ),
        None => {
            let addr = format!("{}:{}", cfg.bind, cfg.port);
            let l = TcpListener::bind(&addr)
                .with_context(|| format!("binding {addr} (in use? change --port)"))?;
            (Listener::Tcp(l), format!("http://{addr}"))
        }
    };
    let approvals = tole_cli::approvals::ApprovalQueue::shared();
    let state = Arc::new(State {
        sessions: Arc::new(Mutex::new(sessions_keeping_pending(&approvals))),
        approvals,
        allow_patterns: cfg.allow_patterns.clone(),
        profiles: cfg.profiles.clone(),
        plan_mode: cfg.plan_mode,
        memory: cfg.memory.clone(),
        auth: cfg.auth,
        workspace_root,
        sessions_dir: cfg.sessions_dir.clone(),
        turnend: cfg.turnend.clone(),
        live_connections: std::sync::atomic::AtomicUsize::new(0),
        buses: tole_cli::events::BusRegistry::default(),
        streams: tole_cli::events::StreamLimiter::new(tole_cli::events::MAX_STREAMS),
        auth_failures: tole_cli::auth::AuthLimiter::new(),
        uploads: tole_cli::serve_images::UploadStore::from_env(),
        hooks,
        hook_dir,
        audit_pending: Mutex::default(),
        mission: cfg.mission.clone(),
        missions: Arc::default(),
        models: models::ModelCatalog::from_env(),
        turn_events: std::sync::OnceLock::new(),
    });
    if !state.hooks.is_empty() {
        let names: Vec<&str> = state.hooks.keys().map(String::as_str).collect();
        eprintln!("tole serve: webhook routes: {}", names.join(", "));
    }
    eprintln!(
        "tole serve: listening on {shown} ({} allow pattern(s), plan_mode={})",
        cfg.allow_patterns.len(),
        cfg.plan_mode
    );
    // The webhook sender (#911) runs whether or not `--cron` is given: a
    // webhook route can run a job without the ticker.
    if !webhook_out.is_empty() {
        let dir = tole_cli::cron::default_dir()?;
        eprintln!(
            "tole serve: webhook sender for {} destination(s)",
            webhook_out.len()
        );
        tole_cli::cron::webhook::spawn_sender(dir, Arc::clone(&webhook_out));
    }
    // Approval events (#911): when a destination lists them, the approval queue
    // tells a sender thread about each request and each resolution, and a timer
    // expires what nobody answers. Absent, the approval paths are unchanged.
    // The same sender also carries `turn_finished` (#962); its notifier is kept
    // for the turn routes.
    if let Some(turns) = tole_cli::webhook_out::approval_sender::spawn(
        Arc::clone(&webhook_out),
        Arc::clone(&state.approvals),
    )
    .turns
    {
        let _ = state.turn_events.set(turns);
    }
    if cfg.cron {
        let dir = tole_cli::cron::default_dir()?;
        eprintln!("tole serve: cron ticker on {}", dir.display());
        let queue = Arc::clone(&state.approvals);
        std::thread::spawn(move || tole_cli::cron::serve_ticker(&dir, queue));
    }
    use std::sync::atomic::Ordering;
    loop {
        let Ok(stream) = listener.accept() else {
            continue;
        };
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
}

/// Largest `--token-file`.
const MAX_TOKEN_FILE: u64 = 4096;

/// The serve bearer token (#682): `--token-file` (a regular file, no link
/// or FIFO, at most 4 KiB, surrounding whitespace dropped; an empty file is
/// an error), else `--token`, else `TOLE_SERVE_TOKEN` (blank counts as
/// unset). Errors never carry the token.
///
/// The token becomes a secret held by role, whatever carried it: redacted
/// on egress, and removed from any child's environment.
pub fn serve_token(
    flag: Option<String>,
    file: Option<&std::path::Path>,
    env: Option<String>,
) -> std::result::Result<String, String> {
    serve_token_optional(flag, file, env)?.ok_or_else(|| {
        "refusing to start an unauthenticated server: set TOLE_SERVE_TOKEN, --token-file \
         or --token"
            .to_string()
    })
}

/// [`serve_token`] where having no token at all is not an error: `--auth
/// basic` may run with the Basic user alone (#742). A file or value that is
/// given but unusable is still an error.
pub fn serve_token_optional(
    flag: Option<String>,
    file: Option<&std::path::Path>,
    env: Option<String>,
) -> std::result::Result<Option<String>, String> {
    let from_file = match file {
        Some(p) => {
            let bytes = tole_core::delegation_report::read_plain_file(p, MAX_TOKEN_FILE)
                .map_err(|e| format!("--token-file {}: {e}", p.display()))?
                .ok_or_else(|| format!("--token-file {}: does not exist", p.display()))?;
            let text = String::from_utf8(bytes)
                .map_err(|_| format!("--token-file {}: not UTF-8", p.display()))?;
            if text.trim().is_empty() {
                return Err(format!("--token-file {}: is empty", p.display()));
            }
            Some(text)
        }
        None => None,
    };
    let token = [from_file, flag, env]
        .into_iter()
        .flatten()
        .map(|t| t.trim().to_string())
        .find(|t| !t.is_empty());
    if let Some(t) = &token {
        tole_core::secrets::add_role_secret("TOLE_SERVE_TOKEN", t);
    }
    Ok(token)
}

/// `tole serve-hash` (#742): print the password hash `--auth basic` takes.
/// The password comes from stdin: a pipe gives one line, a terminal is asked
/// twice without echo. It is never an argument (visible to every local
/// process) and never printed; only the hash goes to stdout.
pub fn hash_command(iterations: u32) -> Result<()> {
    use std::io::IsTerminal;
    let password = if std::io::stdin().is_terminal() {
        let first = read_hidden("Password: ")?;
        let again = read_hidden("Repeat: ")?;
        if !tole_cli::auth::ct_eq(first.as_bytes(), again.as_bytes()) {
            anyhow::bail!("the two passwords differ");
        }
        first
    } else {
        let mut line = String::new();
        // One line at most: a runaway pipe must not be slurped.
        std::io::stdin()
            .lock()
            .take(4096)
            .read_line(&mut line)
            .context("reading the password from stdin")?;
        line.trim_end_matches(['\n', '\r']).to_string()
    };
    let hash = tole_cli::auth::PasswordHash::create(password.as_bytes(), iterations)
        .map_err(anyhow::Error::msg)?;
    println!("{hash}");
    Ok(())
}

/// Terminal attributes to put back: restores echo when dropped, so an early
/// return or a panic cannot leave the terminal silent. (A signal that kills the
/// process, Ctrl-C included, is not handled: `stty echo` repairs the terminal.)
#[cfg(unix)]
struct EchoGuard {
    fd: std::os::fd::RawFd,
    saved: libc::termios,
}

#[cfg(unix)]
impl EchoGuard {
    /// Turn echo off on `fd`; `None` when it is not a terminal.
    fn off(fd: std::os::fd::RawFd) -> Option<Self> {
        // SAFETY: plain termios calls on a descriptor with a zeroed struct the
        // kernel fills in; nothing is retained but the saved copy.
        unsafe {
            let mut t: libc::termios = std::mem::zeroed();
            if libc::tcgetattr(fd, &mut t) != 0 {
                return None;
            }
            let mut quiet = t;
            quiet.c_lflag &= !libc::ECHO;
            libc::tcsetattr(fd, libc::TCSANOW, &quiet);
            Some(Self { fd, saved: t })
        }
    }
}

#[cfg(unix)]
impl Drop for EchoGuard {
    fn drop(&mut self) {
        // SAFETY: restores the attributes read in `off`.
        unsafe { libc::tcsetattr(self.fd, libc::TCSANOW, &self.saved) };
    }
}

/// Read one line from the terminal with echo off (restored on every exit).
fn read_hidden(prompt: &str) -> Result<String> {
    #[cfg(unix)]
    let _echo = {
        use std::os::fd::AsRawFd;
        EchoGuard::off(std::io::stdin().as_raw_fd())
    };
    #[cfg(not(unix))]
    eprintln!("(this platform cannot hide the typed password; pipe it on stdin instead)");
    eprint!("{prompt}");
    let mut line = String::new();
    let read = std::io::stdin().lock().take(4096).read_line(&mut line);
    eprintln!();
    read.context("reading the password")?;
    Ok(line.trim_end_matches(['\n', '\r']).to_string())
}

// ---------------------------------------------------------------------------
// Connection handling
// ---------------------------------------------------------------------------

struct Request {
    method: String,
    path: String,
    authorization: Option<String>,
    /// The verdict `read_request` already reached for this header (only an
    /// image upload is checked while reading), so it is hashed once.
    verdict: Option<bool>,
    /// `Last-Event-ID`: where an SSE client left off (`EventSource` sends it
    /// on reconnect).
    last_event_id: Option<String>,
    /// Raw body bytes (an image upload is binary; JSON routes decode it).
    body: Vec<u8>,
    /// Signature and delivery headers of a webhook (#582).
    hook: tole_core::webhook::Headers,
}

/// Why a request could not be read; decides the response (if any).
enum ReadError {
    /// Connection-level failure or timeout: nothing useful to answer.
    Io,
    /// Syntactically unacceptable request: answered with 400.
    Bad(&'static str),
    /// Declared body over the ceiling. Carries the `Authorization` header
    /// so the caller authenticates (and rate-limits) before answering: a
    /// 413 only for the right token, otherwise the same 401 a small body
    /// gets. The ceiling depends on the token, so answering 413 before
    /// auth would be a token-validity oracle outside the limiter (#558).
    TooLarge {
        authorization: Option<String>,
        verdict: Option<bool>,
    },
    /// `/hooks/<route>` for a route that does not exist (404, body unread).
    NoHookRoute,
    /// A webhook body over its route's cap (413, body unread). Webhooks do
    /// not use the token, so this is no token oracle.
    HookTooLarge,
}

/// The route of a `/hooks/<route>` path.
fn hook_target(path: &str) -> Option<&str> {
    path.strip_prefix("/hooks/")
        .map(|r| r.split('?').next().unwrap_or(r))
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
    stream: &'a Conn,
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

fn read_request(
    stream: &Conn,
    auth: &tole_cli::auth::Authenticator,
    limited: bool,
    hooks: &std::collections::BTreeMap<String, tole_cli::hooks::Route>,
) -> Result<Request, ReadError> {
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
    let mut hook = tole_core::webhook::Headers::default();
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
        } else if name.eq_ignore_ascii_case("x-tole-timestamp") {
            hook.timestamp = Some(value.to_string());
        } else if name.eq_ignore_ascii_case("x-tole-signature") {
            hook.signature = Some(value.to_string());
        } else if name.eq_ignore_ascii_case("x-tole-delivery") {
            hook.delivery = Some(value.to_string());
        } else if name.eq_ignore_ascii_case("x-hub-signature-256") {
            hook.github_signature = Some(value.to_string());
        } else if name.eq_ignore_ascii_case("x-github-delivery") {
            hook.github_delivery = Some(value.to_string());
        } else if name.eq_ignore_ascii_case("x-github-event")
            || name.eq_ignore_ascii_case("x-tole-event")
        {
            hook.event = Some(value.to_string());
        } else if name.eq_ignore_ascii_case("expect") && value.eq_ignore_ascii_case("100-continue")
        {
            expect_continue = true;
        }
    }
    let content_length = content_length.unwrap_or(0);
    // A webhook: an unknown route or a body over the route's cap is
    // answered before a byte of the body is read (#582).
    if let Some(route) = hook_target(&path) {
        let cap = hooks.get(route).ok_or(ReadError::NoHookRoute)?.max_body;
        if content_length > cap {
            return Err(ReadError::HookTooLarge);
        }
    }
    // The larger upload ceiling only for an image upload that already
    // carries a valid credential: pre-auth buffering stays at MAX_BODY_BYTES.
    // An IP over its failure budget is not even checked (no hashing for it).
    let verdict = (method == "POST" && upload_target(&path).is_some() && !limited)
        .then(|| auth.check(authorization.as_deref()));
    let upload = verdict == Some(true);
    let ceiling = if upload {
        MAX_UPLOAD_BYTES
    } else {
        MAX_BODY_BYTES
    };
    if content_length > ceiling {
        // Refuse rather than buffer it.
        return Err(ReadError::TooLarge {
            authorization,
            verdict,
        });
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
        verdict,
        last_event_id,
        body,
        hook,
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

fn respond(stream: &mut Conn, status: u16, payload: &serde_json::Value) {
    respond_with(stream, status, payload, "");
}

/// The 401 of every refused request: one body whatever was wrong (no user
/// or password verdict), and the Basic challenge when Basic is on.
fn respond_unauthorized(stream: &mut Conn, auth: &tole_cli::auth::Authenticator) {
    let extra = auth
        .challenge()
        .map(|c| format!("WWW-Authenticate: {c}\r\n"))
        .unwrap_or_default();
    respond_with(stream, 401, &json!({"error": "unauthorized"}), &extra);
}

/// `extra` is zero or more complete `Name: value\r\n` header lines.
fn respond_with(stream: &mut Conn, status: u16, payload: &serde_json::Value, extra: &str) {
    let body = payload.to_string();
    let reason = reason_phrase(status);
    let head = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\n{extra}Content-Length: {}\r\nConnection: close\r\n\r\n",
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

fn handle_conn(mut stream: Conn, state: Arc<State>) {
    let _guard = ConnGuard(Arc::clone(&state));
    let peer_ip = stream.peer_ip();
    let limited = peer_ip.is_some_and(|ip| state.auth_failures.is_limited(ip));
    let req = match read_request(&stream, &state.auth, limited, &state.hooks) {
        Ok(r) => r,
        Err(ReadError::Io) => return,
        Err(ReadError::Bad(msg)) => {
            respond(&mut stream, 400, &json!({"error": msg}));
            return;
        }
        Err(ReadError::TooLarge {
            authorization,
            verdict,
        }) => {
            // Authenticate first, through the same limiter as every other
            // request, so the answer never tells a wrong token from a right
            // one without a recorded failure.
            if limited {
                respond(
                    &mut stream,
                    429,
                    &json!({"error": "too many failed auth attempts"}),
                );
            } else if !verdict.unwrap_or_else(|| state.auth.check(authorization.as_deref())) {
                if let Some(ip) = peer_ip {
                    state.auth_failures.record_failure(ip);
                }
                respond_unauthorized(&mut stream, &state.auth);
            } else {
                respond(
                    &mut stream,
                    413,
                    &json!({"error": "request body too large"}),
                );
            }
            return;
        }
        Err(ReadError::NoHookRoute) => {
            respond(&mut stream, 404, &json!({"error": "no such hook route"}));
            return;
        }
        Err(ReadError::HookTooLarge) => {
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

    // Rate-limit: refuse when this IP has too many recent auth failures.
    if limited {
        respond(
            &mut stream,
            429,
            &json!({"error": "too many failed auth attempts"}),
        );
        return;
    }

    // Webhooks (#582) authenticate by signature, never by the token.
    if req.method == "POST" {
        if let Some(route) = hook_target(&req.path) {
            let (status, payload) = handle_hook(&state, route, &req, peer_ip);
            respond(&mut stream, status, &payload);
            return;
        }
    }

    // The one auth check (#742): every route below this line is behind it,
    // for a bearer token and for Basic alike.
    let authorized = req.path == "/health"
        || req
            .verdict
            .unwrap_or_else(|| state.auth.check(req.authorization.as_deref()));
    if !authorized {
        if let Some(ip) = peer_ip {
            state.auth_failures.record_failure(ip);
        }
        respond_unauthorized(&mut stream, &state.auth);
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
fn respond_image(stream: &mut Conn, mime: &str, data: &[u8]) {
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

fn write_all(stream: &mut Conn, text: &str) -> bool {
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
    mut stream: Conn,
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
    let mut replay_err_logged = false;
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
    let send_approvals = |stream: &mut Conn, approvals: &mut ApprovalWatch| -> Option<usize> {
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
        // Durable frames (a read error just retries on the next pass).
        let polled = match tail.poll() {
            Ok(entries) => {
                for e in entries {
                    if !write_all(&mut stream, &entry_frame(&e)) {
                        return;
                    }
                    hb.note_write(Instant::now());
                }
                true
            }
            Err(kind) => {
                // Once per stream, kind only (no path), so a recurrence is
                // diagnosable without a line every 150 ms pass.
                if !replay_err_logged {
                    replay_err_logged = true;
                    eprintln!("tole serve: event stream replay could not read the log ({kind})");
                }
                false
            }
        };
        // `ready` says the replay is complete, so it waits for a pass that
        // actually read the log: after a failed read it would announce an
        // empty replay and a client resuming from `after` would lose every
        // entry it missed (lossless replay is the point of the log).
        if !ready_sent && polled {
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
    profile: Option<&SessionProfile>,
    record_profile: bool,
    plan_mode: bool,
) -> Result<(), (u16, serde_json::Value)> {
    // Issue #200: non-preauthorized Writes queue for a remote
    // decision (deny now, resumable; allow = one-shot replay).
    // Queue-time storage path (cora MAJOR): an evicted session's
    // audit fallback must reopen the EXACT durable file — with
    // no --sessions-dir the log lives under the session's cwd.
    let storage_dir = serve_storage_dir(state, cwd);
    // The session's allow list and memory room (#740): the profile's, by the
    // precedence in `serve_support::profiles`; unchanged without a profile.
    let allow = match profile {
        Some(p) => p.allow.clone(),
        None => state.allow_patterns.clone(),
    };
    let memory = match profile {
        Some(p) => p.memory(
            state.memory.as_ref(),
            std::env::var(tole_core::child_env::MEMORY_ROOM)
                .ok()
                .as_deref(),
        ),
        None => state.memory.clone(),
    };
    let approver = tole_cli::approvals::QueueApprover::new(
        allow.clone(),
        Arc::clone(&state.approvals),
        session_id,
        storage_dir.join(format!("{session_id}.jsonl")),
    );
    match open_session_with(
        session_id,
        cwd,
        loading,
        // A request can only turn plan mode ON for its session; a
        // process-level plan mode (`--plan-mode`) is never switched off.
        state.plan_mode || plan_mode,
        approver,
        memory,
        state.sessions_dir.as_deref(),
        state.turnend.clone(),
        allow,
        // No REST cancel endpoint today (#178): a never-fired
        // token keeps serve behavior unchanged.
        tole_core::cancel::CancelToken::default(),
        // No plan UI on the REST face.
        None,
        SessionOverrides {
            system_prompt: profile.and_then(|p| p.system_prompt.clone()),
            model: profile.and_then(|p| p.model.clone()),
            profile: profile.map(|p| p.name.clone()),
        },
    ) {
        Ok(session_state) => {
            // Record the choice in the session log BEFORE the session is
            // live (append-only: a new register entry, #740). A log that
            // cannot take it must not serve a session whose profile is
            // unrecorded.
            if let (Some(p), true) = (profile, record_profile) {
                let mut storage = session_state
                    .storage
                    .lock()
                    .unwrap_or_else(|e| e.into_inner());
                if let Err(e) = tole_core::storage::Storage::commit(
                    &mut *storage,
                    tole_core::storage::Commit::new().register(profiles::record_write(&p.name)),
                ) {
                    return Err((500, json!({"error": format!("recording the profile: {e}")})));
                }
            }
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
            warm_status(state, session_id);
            Ok(())
        }
        // Another process (a TUI, a second serve) holds the writer lock.
        Err(e) if e.contains("open for writing") => Err((409, json!({"error": e}))),
        Err(e) => Err((500, json!({"error": e}))),
    }
}

/// The live-session map for `tole serve`: a session with a pending approval
/// is never evicted, because an Allow resumes it in memory (#979).
fn sessions_keeping_pending(approvals: &Arc<tole_cli::approvals::ApprovalQueue>) -> Sessions {
    let queue = Arc::clone(approvals);
    let mut sessions = Sessions::default();
    sessions.set_keep(Box::new(move |id| queue.has_pending(id)));
    sessions
}

/// Look up a session's context window off the request path (#573): the
/// provider's `/models` may be slow and the status route never waits for it.
fn warm_status(state: &State, id: &str) {
    let st = lock_sessions(&state.sessions).get(id).cloned();
    if let Some(st) = st {
        std::thread::spawn(move || st.warm_status());
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
///
/// `profile` (#740) is the request's per-session profile, already checked to
/// exist. It wins over the one the log recorded; with none requested the
/// recorded one is used again (refused with 409 when it is no longer defined,
/// never silently dropped for the daemon's wider defaults). A live session's
/// profile is fixed for its life: asking for another one is a 409.
fn resume_session(
    state: &State,
    id: &str,
    cwd: &str,
    requested: Option<SessionProfile>,
) -> (u16, serde_json::Value) {
    if tole_cli::session_host::validate_session_id(id).is_none() {
        return (400, json!({"error": "invalid session id"}));
    }
    // Live first: the writer lock is held by this very process, so a second
    // open of the same file would be refused as "locked".
    {
        let sessions = lock_sessions(&state.sessions);
        if let Some(st) = sessions.get(id) {
            if let Some(want) = &requested {
                if st.profile.as_deref() != Some(want.name.as_str()) {
                    return (
                        409,
                        json!({"error": "the session is live under another profile; a session's \
                                profile is fixed while its process runs",
                               "profile": st.profile}),
                    );
                }
            }
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
    // What the log already says (read-only: the writer lock is taken at
    // open). A log that cannot be read, or whose recorded name is not a
    // profile name, is never taken as "no profile": that would resume under
    // the daemon's defaults, which can be wider than the recorded profile.
    let recorded = match tole_core::storage::JsonlStorage::open_read_only(&file) {
        Ok(s) => Ok(profiles::recorded(&s)),
        Err(e) => Err(e.to_string()),
    };
    let profile = match requested {
        Some(p) => Some(p),
        None => match &recorded {
            Ok(profiles::Recorded::None) => None,
            Ok(profiles::Recorded::Name(n)) => match state.profiles.select(n) {
                Ok(p) => Some(p),
                Err(e) => {
                    return (
                        409,
                        json!({"error": format!(
                            "the session was recorded under profile `{n}`, which cannot be used \
                             now ({}); resume with an explicit \"profile\"",
                            e.message()
                        )}),
                    )
                }
            },
            Ok(profiles::Recorded::Invalid) => {
                return (
                    409,
                    json!({"error": "the session log records a profile with an invalid name; \
                            resume with an explicit \"profile\""}),
                )
            }
            Err(_) => {
                return (
                    500,
                    json!({"error": "the session log cannot be read to find the profile it was \
                            recorded under; fix it or resume with an explicit \"profile\""}),
                )
            }
        },
    };
    let record = profile
        .as_ref()
        .is_some_and(|p| recorded != Ok(profiles::Recorded::Name(p.name.clone())));
    match open_serve_session(state, id, cwd, true, profile.as_ref(), record, false) {
        Ok(()) => {
            let mut out = json!({
                "sessionId": id, "workspace": cwd, "resumed": true, "alreadyLive": false
            });
            if let Some(p) = &profile {
                out["profile"] = json!(p.name);
            }
            (200, out)
        }
        Err(resp) => resp,
    }
}

/// An insecure loopback route's delivery: no signature, so the delivery id
/// and the event are taken as sent (still validated).
fn insecure_delivery(
    h: &tole_core::webhook::Headers,
) -> Result<tole_core::webhook::Verified, tole_core::webhook::Reject> {
    use tole_core::webhook::{self, Reject};
    if h.event.as_deref().is_some_and(|e| !webhook::valid_event(e)) {
        return Err(Reject::BadEvent);
    }
    let id = h
        .delivery
        .clone()
        .filter(|d| webhook::valid_delivery(d))
        .ok_or(Reject::BadDelivery)?;
    Ok(webhook::Verified {
        key: id.clone(),
        delivery: id,
        event: h.event.clone(),
    })
}

/// `POST /hooks/<route>` (#582): the signature check, then the route's rate
/// limit and the delivery's idempotency, both kept in the cron ledger so
/// they survive a restart. A failed signature counts toward the source's
/// failed-auth limit. Answers never echo a header value but the delivery
/// id, which passed validation.
fn handle_hook(
    state: &State,
    name: &str,
    req: &Request,
    peer: Option<std::net::IpAddr>,
) -> (u16, serde_json::Value) {
    use tole_core::webhook::{self, Reject};
    let Some(route) = state.hooks.get(name) else {
        return (404, json!({"error": "no such hook route"}));
    };
    let now = tole_cli::cron::now();
    let checked = match route.key() {
        Some(key) => webhook::verify(key, &req.hook, &req.body, now),
        // An insecure route answers loopback clients only (and loads only
        // on a loopback bind).
        None if peer.is_some_and(|ip| ip.is_loopback()) => insecure_delivery(&req.hook),
        None => Err(Reject::Unsigned),
    };
    let verified = match checked {
        Ok(v) => v,
        Err(r) => {
            if r.status() == 401 {
                if let Some(ip) = peer {
                    state.auth_failures.record_failure(ip);
                }
            }
            return (r.status(), json!({"error": r.reason()}));
        }
    };
    let Some(dir) = &state.hook_dir else {
        return (503, json!({"error": "no cron dir for webhook state"}));
    };
    let rate = route.rate_per_minute as usize;
    let outcome = tole_cli::cron::store::with_lock(dir, |l| {
        if l.state().hooks_since(name, now - 59) >= rate {
            return Ok(None);
        }
        // #678: the key is signed material (the native delivery id, or the
        // GitHub body's digest), so a capture re-sent under a fresh
        // delivery header is a duplicate.
        Ok(Some(l.record_hook(name, &verified.key, now)?))
    });
    let id = verified.delivery.as_str();
    match outcome {
        Ok(None) => (429, json!({"error": "rate limit for this route"})),
        Ok(Some(false)) => (200, json!({"status": "duplicate", "delivery": id})),
        Ok(Some(true)) => hook_action(state, name, route, req, &verified, dir, now),
        Err(e) => {
            eprintln!("tole serve: webhook state: {e}");
            (500, json!({"error": "webhook state unavailable"}))
        }
    }
}

/// A verified, first-seen delivery (#582 part 2): the events allowlist and
/// the filter run on the payload before any LLM work (`200 ignored`); a
/// match fires the route's `cron_job` through the claim path a tick uses
/// (the #640 trust check, delivery, provider, skills, the daily cap) and
/// answers `202` once the run is claimed. The run gets the fenced, rendered
/// payload as context for this run only, and no Write tools unless the
/// route says `allow_writes`.
fn hook_action(
    state: &State,
    name: &str,
    route: &tole_cli::hooks::Route,
    req: &Request,
    verified: &tole_core::webhook::Verified,
    dir: &std::path::Path,
    now: i64,
) -> (u16, serde_json::Value) {
    use tole_cli::hook_filter;
    let (id, event) = (verified.delivery.as_str(), verified.event.as_deref());
    if !hook_filter::event_allowed(route.events.as_deref(), event) {
        return (
            200,
            json!({"status": "ignored", "reason": "event", "delivery": id}),
        );
    }
    let payload = match serde_json::from_slice::<serde_json::Value>(&req.body) {
        Ok(v) => v,
        Err(_) if route.filter.is_none() && route.template.is_none() => serde_json::Value::Null,
        Err(_) => return (400, json!({"error": "the body is not JSON"})),
    };
    if route
        .filter
        .as_ref()
        .is_some_and(|f| !hook_filter::matches(f, &payload))
    {
        return (
            200,
            json!({"status": "ignored", "reason": "filter", "delivery": id}),
        );
    }
    let Some(job) = &route.cron_job else {
        return (202, json!({"status": "accepted", "delivery": id}));
    };
    let secrets = tole_cli::provider_chain::process_secrets(&tole_cli::config_apply::real_env);
    let rendered = route
        .template
        .as_ref()
        .map(|t| t.render(&payload, &secrets))
        .unwrap_or_default();
    let trigger = tole_cli::cron::tick::Trigger {
        context: hook_filter::fenced(name, event, id, &rendered),
        read_only: !route.allow_writes,
    };
    let opts = tole_cli::cron::TickOptions {
        approvals: Some(Arc::clone(&state.approvals)),
        daily_token_cap: tole_cli::cron::daily_token_cap(),
        webhooks: tole_cli::webhook_out::loaded(),
        ..tole_cli::cron::TickOptions::standalone(1)
    };
    let mut report = tole_cli::cron::TickReport::default();
    match tole_cli::cron::claim_now(dir, job, now, &opts, &mut report) {
        Ok(Some(c)) => {
            let fire = c.fire_id().to_string();
            let claimed = vec![c.with_trigger(trigger)];
            let dir = dir.to_path_buf();
            std::thread::spawn(move || {
                let mut r = tole_cli::cron::TickReport::default();
                if let Err(e) = tole_cli::cron::tick::run_claimed(&dir, claimed, now, &opts, &mut r)
                {
                    eprintln!("tole serve: webhook run: {e}");
                }
            });
            (
                202,
                json!({"status": "accepted", "delivery": id, "fire": fire}),
            )
        }
        Ok(None) => (
            409,
            json!({"error": "the job did not run (blocked or out of budget; see tole cron list)", "delivery": id}),
        ),
        Err(e) => (409, json!({"error": e.to_string(), "delivery": id})),
    }
}

fn route(state: &State, method: &str, path: &str, body: &str) -> (u16, serde_json::Value) {
    match (method, path) {
        ("GET", "/health") => (
            200,
            json!({
                "status": "ok",
                "protocol": tole_cli::protocol::SERVE_PROTOCOL_VERSION,
                "version": env!("CARGO_PKG_VERSION"),
            }),
        ),
        ("GET", "/models") => models::list_route(state),
        ("GET", "/sessions") => {
            let sessions = lock_sessions(&state.sessions);
            let list: Vec<_> = sessions
                .iter()
                .map(|(id, st)| {
                    let mut v = json!({
                        "id": id,
                        "busy": *st.busy.lock().expect("busy lock"),
                    });
                    if let Some(p) = &st.profile {
                        v["profile"] = json!(p);
                    }
                    v
                })
                .collect();
            (200, json!({"sessions": list}))
        }
        ("GET", p) if p == "/sessions/index" || p.starts_with("/sessions/index?") => {
            // Past sessions of this workspace (#572); the route is before
            // `/sessions/{id}` so `index` is never read as an id.
            let query = p.split_once('?').map_or("", |(_, q)| q);
            let dir = match &state.sessions_dir {
                Some(d) => d.clone(),
                None => state.workspace_root.join(".tole/sessions"),
            };
            let live = |id: &str| {
                lock_sessions(&state.sessions)
                    .get(id)
                    .map(|st| *st.busy.lock().expect("busy lock"))
            };
            past_sessions::list(
                &state.workspace_root,
                &dir,
                state.sessions_dir.is_none(),
                query,
                past_sessions::LIMITS,
                &live,
            )
        }
        ("POST", "/sessions") => {
            let Ok(req) = serde_json::from_str::<serde_json::Value>(body) else {
                return (400, json!({"error": "invalid JSON body"}));
            };
            // The memory room belongs to the profile (#740): a request that
            // names one is refused, never half-honored.
            if req.get("memory_room").is_some() {
                return (
                    400,
                    json!({"error": "memory_room cannot be set by a request: it comes from the \
                            session's profile"}),
                );
            }
            let profile = match req.get("profile") {
                None | Some(serde_json::Value::Null) => None,
                Some(serde_json::Value::String(name)) => match state.profiles.select(name) {
                    Ok(p) => Some(p),
                    Err(e) => return (e.status(), json!({"error": e.message()})),
                },
                Some(_) => {
                    return (
                        400,
                        json!({"error": "profile must be a profile name string"}),
                    )
                }
            };
            // Plan mode (#947): the same registry filter as `--plan-mode`
            // (mutation tools absent). A request may turn it on for this
            // session; there is no way to turn a process-level one off.
            let plan_mode = match req.get("plan_mode") {
                None | Some(serde_json::Value::Null) => false,
                Some(serde_json::Value::Bool(b)) => *b,
                Some(_) => return (400, json!({"error": "plan_mode must be a boolean"})),
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
                Some(serde_json::Value::String(id)) => {
                    if plan_mode {
                        return (
                            400,
                            json!({"error": "plan_mode applies when a session is created, not \
                                    when one is resumed"}),
                        );
                    }
                    return resume_session(state, id, cwd, profile);
                }
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
            match open_serve_session(
                state,
                &session_id,
                cwd,
                false,
                profile.as_ref(),
                true,
                plan_mode,
            ) {
                Ok(()) => {
                    let mut out = json!({"sessionId": session_id, "workspace": cwd});
                    if plan_mode || state.plan_mode {
                        out["planMode"] = json!(true);
                    }
                    if let Some(p) = &profile {
                        out["profile"] = json!(p.name);
                    }
                    (200, out)
                }
                Err(resp) => resp,
            }
        }
        // Missions (#947): a mission is its own session, run on a thread.
        ("POST", "/missions") => missions::start(state, body),
        ("GET", p) if p.starts_with("/missions/") && !p["/missions/".len()..].contains('/') => {
            missions::status(state, &p["/missions/".len()..])
        }
        ("POST", p) if p.starts_with("/missions/") && p.ends_with("/cancel") => {
            let id = &p["/missions/".len()..p.len() - "/cancel".len()];
            missions::cancel(state, id)
        }
        // Cron run results (#579, design §9): the pull delivery target.
        ("GET", p) if p == "/cron/runs" || p.starts_with("/cron/runs?") => {
            let query = |key: &str| {
                p.split_once('?').and_then(|(_, q)| {
                    q.split('&')
                        .find_map(|kv| kv.strip_prefix(key)?.strip_prefix('='))
                })
            };
            let limit = match query("limit").map(str::parse::<usize>) {
                None => None,
                Some(Ok(n)) => Some(n),
                Some(Err(_)) => return (400, json!({"error": "limit must be a number"})),
            };
            let runs = tole_cli::cron::default_dir()
                .and_then(|dir| tole_cli::cron::runs_json(&dir, query("job"), limit));
            match runs {
                Ok(v) => (200, v),
                Err(e) if e.to_string().starts_with("no job") => {
                    (404, json!({"error": e.to_string()}))
                }
                Err(e) => (500, json!({"error": format!("{e:#}")})),
            }
        }
        // Remote approvals (issue #200): list + decide.
        ("GET", p) if p == "/approvals" || p.starts_with("/approvals?") => {
            let session = p.split_once('?').and_then(|(_, q)| {
                q.split('&')
                    .find_map(|kv| kv.strip_prefix("session=").map(str::to_string))
            });
            let mut list = state
                .approvals
                .list(session.as_deref())
                .iter()
                .map(tole_cli::approvals::approval_json)
                .collect::<Vec<_>>();
            // The durable queue of delegated children (#714): no serve
            // session owns those, so a `?session=` filter leaves them out.
            if session.is_none() {
                if let Some(data) = tole_core::paths::tole_data_dir() {
                    let sx = tole_cli::provider_chain::process_secrets(
                        &tole_cli::config_apply::real_env,
                    );
                    let now = tole_cli::delegated_approver::real_now();
                    list.extend(tole_cli::serve_delegation::list(&data, &sx, now));
                }
            }
            (200, json!({"approvals": list}))
        }
        (method, p)
            if method == "POST" && (p.starts_with("/approvals/") && p.ends_with("/decision")) =>
        {
            let id = p
                .trim_start_matches("/approvals/")
                .trim_end_matches("/decision")
                .to_string();
            // A delegated child's request (#714): the durable queue, not
            // serve's in-memory one. The child is already waiting: no resume.
            if id.starts_with("dap-") {
                let sx =
                    tole_cli::provider_chain::process_secrets(&tole_cli::config_apply::real_env);
                return tole_cli::serve_delegation::decide_route(
                    tole_core::paths::tole_data_dir().as_deref(),
                    &sx,
                    tole_cli::delegated_approver::real_now(),
                    &tole_cli::config_apply::real_env,
                    &id,
                    body,
                );
            }
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
                    transitioned,
                    ..
                } => {
                    let same = match status {
                        tole_cli::approvals::ApprovalStatus::Approved => allow,
                        tole_cli::approvals::ApprovalStatus::Denied => !allow,
                        _ => false,
                    };
                    // Not the first decision (a repeat, a flip after the fact,
                    // or an entry that had already expired): nothing is audited
                    // and nothing resumes, so the durable trail keeps the
                    // verdict that took effect and the one-shot is not replayed
                    // twice. The same verdict again is idempotent, a different
                    // one is a conflict. The one exception is a first decision
                    // whose audit write failed: its retry finishes the job.
                    let owes_audit = !transitioned
                        && same
                        && state
                            .audit_pending
                            .lock()
                            .unwrap_or_else(|p| p.into_inner())
                            .contains(&id);
                    if !transitioned && !owes_audit {
                        return if same {
                            (
                                200,
                                json!({"id": id, "status": status.as_str(), "resuming": false}),
                            )
                        } else {
                            (
                                409,
                                json!({
                                    "error": format!("approval already {}", status.as_str()),
                                    "id": id,
                                    "status": status.as_str(),
                                }),
                            )
                        };
                    }
                    (session_id, tool, status)
                }
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
            {
                let mut pending = state
                    .audit_pending
                    .lock()
                    .unwrap_or_else(|p| p.into_inner());
                if audit.is_err() {
                    pending.insert(id.clone());
                } else {
                    pending.remove(&id);
                }
            }
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
                // once (a repeat returns above, without a resume) and pending
                // approvals only come
                // from the capped, one-turn-per-session map.
                let sessions = Arc::clone(&state.sessions);
                let sid = session_id.clone();
                // A session this server does not host is a parked cron run
                // (#581): it resumes through the cron ledger instead.
                let serve_session = lock_sessions(&state.sessions).get(&sid).is_some();
                // Neither live here nor parked in the cron ledger: nothing
                // can run the approved call (the session was evicted or its
                // process restarted). Say so instead of promising a resume.
                // An error reading the ledger is not a "no": the resume below
                // surfaces it in the log, as before.
                if !serve_session && matches!(tole_cli::cron::has_parked_run(&sid), Ok(false)) {
                    // The grant must not outlive the refusal: a session
                    // reopened later would consume it silently.
                    state.approvals.revoke_one_shot(&id);
                    return (
                        409,
                        json!({
                            "error": "the decision is recorded, but the session is not live in \
                                      this server and no cron run is parked on it, so nothing \
                                      was resumed; reopen it with POST /sessions {\"resume\": \
                                      \"<id>\"} and send a new prompt",
                            "id": id,
                            "status": status.as_str(),
                            "resuming": false,
                        }),
                    );
                }
                let queue = Arc::clone(&state.approvals);
                let turn_notifier = state.turn_events.get().cloned();
                std::thread::spawn(move || {
                    if serve_session {
                        match tole_cli::session_host::resume_session_turn_with_end(
                            Arc::clone(&sessions),
                            &sid,
                            Some(&queue),
                        ) {
                            Ok(r) => announce_turn(
                                turn_notifier.as_ref(),
                                &queue,
                                &sid,
                                r.turn_end,
                                r.approval_tool.as_deref(),
                                r.queue_mark,
                            ),
                            Err(e) => {
                                eprintln!("tole serve: approval resume failed: {}", e.message);
                                announce_turn(
                                    turn_notifier.as_ref(),
                                    &queue,
                                    &sid,
                                    e.end,
                                    None,
                                    None,
                                );
                            }
                        }
                        let st = lock_sessions(&sessions).get(&sid).cloned();
                        if let Some(st) = st {
                            st.warm_status();
                        }
                        return;
                    }
                    match tole_cli::cron::resume_after_approval(queue, &sid) {
                        Ok(Some((fire, status))) => {
                            eprintln!("tole serve: cron run {fire} resumed: {status:?}")
                        }
                        Ok(None) => eprintln!("tole serve: no parked cron run for session {sid}"),
                        Err(e) => eprintln!("tole serve: cron resume failed: {e}"),
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
                    let (busy, entries, report, snapshot, stale) = {
                        let sessions = lock_sessions(&state.sessions);
                        let Some(st) = sessions.get(id) else {
                            return (404, json!({"error": "unknown session"}));
                        };
                        let busy = *st.busy.lock().expect("busy lock");
                        let row = match st.storage.try_lock() {
                            // Idle: a live read, which also refreshes the
                            // snapshot a later busy answer falls back to.
                            Ok(guard) => (
                                busy,
                                Some(guard.entries().len()),
                                // Issue #211: additive usage report.
                                tole_cli::usage_report::usage_json(guard.entries(), guard.usages()),
                                Some(st.refresh_status(&*guard)),
                                false,
                            ),
                            // Turn in flight: skip the count and report and
                            // answer from the last-known snapshot (#573),
                            // which never needs the turn's lock.
                            Err(_) => (busy, None, json!(null), st.last_status(), true),
                        };
                        row
                    };
                    let entries = match entries {
                        Some(n) => json!(n),
                        None => json!(null),
                    };
                    let mut out = json!({
                        "id": id,
                        "entries": entries,
                        "busy": busy,
                        "usage_report": report,
                    });
                    if let (Some(o), Some(f)) = (
                        out.as_object_mut(),
                        tole_cli::status::StatusSnapshot::json_fields(snapshot.as_ref(), stale)
                            .as_object(),
                    ) {
                        o.extend(f.clone());
                    }
                    (200, out)
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
                    let turn = run_session_turn_with_end(
                        Arc::clone(&state.sessions),
                        id,
                        text,
                        &images,
                        observer,
                        // The turn snapshots the queue once it holds the
                        // session (#571); a session runs one turn at a time
                        // (a second prompt is a 409 before any lookup), so
                        // an entry of this session newer than the snapshot
                        // was queued by THIS turn.
                        Some(&state.approvals),
                    );
                    // The first turn gives the session a prompt to measure:
                    // learn the context window off this request path (#573).
                    warm_status(state, id);
                    match &turn {
                        Ok(r) => announce_turn(
                            state.turn_events.get(),
                            &state.approvals,
                            id,
                            r.turn_end,
                            r.approval_tool.as_deref(),
                            r.queue_mark,
                        ),
                        Err(e) => announce_turn(
                            state.turn_events.get(),
                            &state.approvals,
                            id,
                            e.end,
                            None,
                            None,
                        ),
                    }
                    match turn {
                        // A turn waiting for an approval is not a refusal
                        // (#571). A stop that queued nothing (a pre-tool hook
                        // denial) is one: nothing waits for a decision.
                        Ok(r) => {
                            let approval = r.approval_tool.as_deref().zip(r.queue_mark).and_then(
                                |(tool, mark)| {
                                    tole_cli::approvals::approval_required_body(
                                        &state.approvals,
                                        id,
                                        tool,
                                        mark,
                                    )
                                },
                            );
                            match (approval, r.text) {
                                (Some(body), _) => (200, body),
                                (None, Some(t)) => (200, json!({"stopReason": r.stop, "text": t})),
                                (None, None) => (200, json!({"stopReason": r.stop})),
                            }
                        }
                        Err(e) if e.message.contains("session is busy") => {
                            (409, json!({"error": e.message}))
                        }
                        Err(e) => (500, json!({"error": e.message})),
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
                ("POST", id, Some("fork")) => fork_route(state, id, body),
                ("GET", id, Some("tree")) => tree_route(state, id),
                ("POST", id, Some("compact")) => compact_route(state, id),
                ("POST", id, Some("name")) => name_route(state, id, body),
                ("POST", id, Some("model")) => models::model_route(state, id, body),
                ("POST", id, Some("effort")) => models::effort_route(state, id, body),
                _ => (404, json!({"error": "not found"})),
            }
        }
        _ => (404, json!({"error": "not found"})),
    }
}

/// `POST /sessions/{id}/fork {"at"?: <seq>}` (#947): fork the session at a
/// settled entry into a new session file, through the same
/// `JsonlStorage::fork` the TUI `/fork` and `tole branch` call. `at` is the
/// `seq` of an entry as `GET .../entries` shows it (default: the last
/// settled entry). Writes only the new file and, as the TUI does, one
/// index line; the source is never touched. The new session is not made
/// live: `POST /sessions {"resume": "<new id>"}` opens it.
///
/// 404 unknown session, 409 while a turn runs (the turn holds the storage
/// lock; a fork waits for none), 400 for a bad or unsettled `at`.
fn fork_route(state: &State, id: &str, body: &str) -> (u16, serde_json::Value) {
    let req = if body.trim().is_empty() {
        json!({})
    } else {
        match serde_json::from_str::<serde_json::Value>(body) {
            Ok(v) if v.is_object() => v,
            _ => return (400, json!({"error": "invalid JSON body"})),
        }
    };
    let at_seq = match req.get("at") {
        None | Some(serde_json::Value::Null) => None,
        Some(v) => match v.as_u64() {
            Some(n) => Some(n),
            None => return (400, json!({"error": "at must be an entry seq (integer)"})),
        },
    };
    let st = {
        let sessions = lock_sessions(&state.sessions);
        sessions.get(id).cloned()
    };
    let Some(st) = st else {
        return (404, json!({"error": "unknown session"}));
    };
    if *st.busy.lock().expect("busy lock") {
        return (409, json!({"error": "session is busy: a turn is running"}));
    }
    // The turn lock, held across the whole fork as the TUI holds it, so no
    // turn appends to the source while it is read. A turn that took it
    // between the check above and here is the same 409.
    let Ok(guard) = st.storage.try_lock() else {
        return (409, json!({"error": "session is busy: a turn is running"}));
    };
    let at_id = match at_seq {
        None => None,
        Some(n) => match guard.entries().iter().find(|e| e.seq == n) {
            Some(e) => Some(e.id.clone()),
            None => return (400, json!({"error": format!("no entry with seq {n}")})),
        },
    };
    let Some(dir) = st.path.parent().map(|d| d.to_path_buf()) else {
        return (500, json!({"error": "session file has no directory"}));
    };
    let new_id = tole_cli::session_host::new_session_id("s");
    let forked =
        match tole_core::storage::JsonlStorage::fork(&st.path, &dir, &new_id, at_id.as_deref()) {
            Ok(f) => f,
            Err(tole_core::storage::StorageError::Invalid(m)) => {
                return (400, json!({"error": m}));
            }
            Err(e) => return (500, json!({"error": format!("forking {id}: {e}")})),
        };
    let cut = forked
        .forked_from()
        .map(|o| o.entry.clone())
        .unwrap_or_default();
    let cut_seq = guard.entries().iter().find(|e| e.id == cut).map(|e| e.seq);
    drop(forked);
    drop(guard);
    tole_cli::session_index::record(&dir, &new_id, "branch");
    eprintln!("tole: session/{id} forked into {new_id} (serve)");
    (
        200,
        json!({"id": new_id, "forkedFrom": id, "at": cut, "seq": cut_seq}),
    )
}

/// `GET /sessions/{id}/tree` (#947): the session's family, from the same
/// reader as `tole sessions --tree` and the TUI `/tree` (its parent chain
/// up to the root and every fork below it), read from the session files
/// so it works while a turn runs. Rows are depth-first, `depth` relative
/// to the root. 404 for an unknown session.
fn tree_route(state: &State, id: &str) -> (u16, serde_json::Value) {
    let path = {
        let sessions = lock_sessions(&state.sessions);
        sessions.get(id).map(|st| st.path.clone())
    };
    let Some(path) = path else {
        return (404, json!({"error": "unknown session"}));
    };
    let Some(dir) = path.parent() else {
        return (500, json!({"error": "session file has no directory"}));
    };
    let rows: Vec<_> =
        tole_cli::session_index::family(tole_cli::session_index::tree_nodes(dir), id)
            .into_iter()
            .map(|(depth, n)| {
                json!({
                    "id": n.id,
                    "parent": n.parent,
                    "at": n.at,
                    "state": n.pc,
                    "turns": n.turns,
                    "depth": depth,
                    "current": n.id == id,
                })
            })
            .collect();
    (200, json!({"id": id, "tree": rows}))
}

/// Run `write` on session `id`'s storage under the turn lock, for the routes
/// that write a `fact/*` register the TUI writes too (#947). 404 for an
/// unknown session, 409 while a turn runs (the busy flag, then the turn
/// lock itself: a turn that took it in between is the same 409).
fn with_idle_session(
    state: &State,
    id: &str,
    write: impl FnOnce(&mut tole_core::storage::JsonlStorage) -> Result<(), (u16, String)>,
) -> Result<(), (u16, serde_json::Value)> {
    let st = {
        let sessions = lock_sessions(&state.sessions);
        sessions.get(id).cloned()
    };
    let Some(st) = st else {
        return Err((404, json!({"error": "unknown session"})));
    };
    let busy = || (409, json!({"error": "session is busy: a turn is running"}));
    if *st.busy.lock().expect("busy lock") {
        return Err(busy());
    }
    let Ok(mut guard) = st.storage.try_lock() else {
        return Err(busy());
    };
    write(&mut guard).map_err(|(status, e)| (status, json!({"error": e})))?;
    // A register changed: keep the last-known status snapshot current (#573).
    st.refresh_status(&*guard);
    Ok(())
}

/// `POST /sessions/{id}/compact` (#947): the TUI `/compact`, i.e.
/// `session_settings::request_compaction`. It records the forced pass of
/// #481 and the session's NEXT prompt compacts first (when compaction is
/// enabled); it is not a mid-turn compaction and runs no model call here.
fn compact_route(state: &State, id: &str) -> (u16, serde_json::Value) {
    match with_idle_session(state, id, |s| {
        tole_cli::session_settings::request_compaction(s).map_err(|e| (500, e))
    }) {
        Ok(()) => {
            eprintln!("tole: session/{id} compaction requested (serve)");
            (200, json!({"id": id, "compaction": "requested"}))
        }
        Err(e) => e,
    }
}

/// `POST /sessions/{id}/name {"name": "<label>"}` (#947): the TUI `/name`,
/// i.e. `session_settings::set_name` (the `fact/session_name` register).
fn name_route(state: &State, id: &str, body: &str) -> (u16, serde_json::Value) {
    let Ok(req) = serde_json::from_str::<serde_json::Value>(body) else {
        return (400, json!({"error": "invalid JSON body"}));
    };
    let Some(name) = req.get("name").and_then(|v| v.as_str()) else {
        return (400, json!({"error": "missing name"}));
    };
    // The same label rule as the TUI, checked before the session is looked
    // at so a bad name is a 400 whatever the session's state.
    let name = match tole_cli::session_settings::clean_label("name", name) {
        Ok(n) => n,
        Err(e) => return (400, json!({"error": e})),
    };
    match with_idle_session(state, id, |s| {
        tole_cli::session_settings::set_name(s, &name).map_err(|e| (500, e))
    }) {
        Ok(()) => (200, json!({"id": id, "name": name})),
        Err(e) => e,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpStream;

    /// #682: the token comes from a file first, then the flag, then the
    /// environment; an empty file is an error, and no error shows a token.
    #[test]
    fn the_serve_token_prefers_the_file_and_never_echoes_a_value() {
        let dir = crate::test_dir::TestDir::new("tole-serve-token");
        let file = dir.join("token");
        std::fs::write(&file, "file-token-0682\n").unwrap();
        let s = |v: &str| Some(v.to_string());
        assert_eq!(
            serve_token(None, Some(&file), s("env-token")).unwrap(),
            "file-token-0682"
        );
        assert_eq!(
            tole_core::secrets::process_secrets().redact("t=file-token-0682"),
            "t=«TOLE_SERVE_TOKEN»",
            "the token is a secret held by role"
        );
        assert_eq!(
            serve_token(s(" flag-token "), None, s("env-token")).unwrap(),
            "flag-token"
        );
        assert_eq!(
            serve_token(s("  "), None, s("env-token")).unwrap(),
            "env-token"
        );
        let empty = dir.join("empty");
        std::fs::write(&empty, "\n").unwrap();
        let e = serve_token(None, Some(&empty), s("env-token-0682")).unwrap_err();
        assert!(e.contains("is empty") && !e.contains("env-token"), "{e}");
        let e = serve_token(None, Some(&dir.join("missing")), None).unwrap_err();
        assert!(e.contains("does not exist"), "{e}");
        let e = serve_token(None, None, s(" ")).unwrap_err();
        assert!(e.contains("TOLE_SERVE_TOKEN"), "{e}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn reason_phrases_cover_every_status_the_routes_emit() {
        assert_eq!(reason_phrase(202), "Accepted");
        assert_eq!(reason_phrase(429), "Too Many Requests");
        assert_eq!(reason_phrase(503), "Service Unavailable");
        assert_eq!(reason_phrase(413), "Payload Too Large");
        assert_eq!(reason_phrase(403), "Forbidden");
    }

    fn test_state(root: std::path::PathBuf, sessions_dir: std::path::PathBuf) -> State {
        // The entrypoint fixes the process's skills choice before any
        // session opens (#945); these tests load none.
        tole_cli::session_host::set_server_skills(
            tole_cli::session::assemble::ServerSkills::new(vec![], true).unwrap(),
        );
        tole_cli::session::owner_mcp::set(vec![]).unwrap();
        State {
            sessions: Arc::new(Mutex::new(Sessions::default())),
            approvals: tole_cli::approvals::ApprovalQueue::shared(),
            allow_patterns: vec![],
            profiles: Default::default(),
            plan_mode: false,
            memory: None,
            auth: tole_cli::auth::Authenticator::token("t".into()),
            workspace_root: root,
            sessions_dir: Some(sessions_dir),
            turnend: vec![],
            live_connections: std::sync::atomic::AtomicUsize::new(0),
            buses: tole_cli::events::BusRegistry::default(),
            streams: tole_cli::events::StreamLimiter::new(tole_cli::events::MAX_STREAMS),
            auth_failures: tole_cli::auth::AuthLimiter::new(),
            uploads: None,
            hooks: Default::default(),
            hook_dir: None,
            audit_pending: Mutex::default(),
            mission: Default::default(),
            missions: Arc::default(),
            models: Default::default(),
            turn_events: std::sync::OnceLock::new(),
        }
    }

    /// A first decision whose durable audit write failed (the client got a
    /// 5xx) is finished by a retry of the same verdict: the audit is written
    /// and the resume starts then, once. The opposite verdict is still a
    /// conflict, and once the audit is in, a repeat is a plain 200.
    #[test]
    fn a_decision_whose_audit_failed_is_finished_by_its_retry() {
        let base = crate::test_dir::TestDir::new("tole-serve-audit-retry");
        let root = base.join("ws");
        let sessions = base.join("sessions");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir_all(&sessions).unwrap();
        let state = test_state(root, sessions);
        // The session file's directory does not exist yet: the audit cannot
        // be written.
        let missing = base.join("later");
        let id = state.approvals.queue_entry(
            "srv-audit-retry",
            "write_file",
            "write file x.txt",
            7,
            missing.join("srv-audit-retry.jsonl"),
        );
        let path = format!("/approvals/{id}/decision");
        let allow = json!({"decision": "allow"}).to_string();
        let deny = json!({"decision": "deny"}).to_string();

        let (status, body) = route(&state, "POST", &path, &allow);
        assert_eq!(status, 500, "{body}");
        // The verdict took effect in the queue; the opposite one conflicts.
        let (status, body) = route(&state, "POST", &path, &deny);
        assert_eq!(status, 409, "{body}");
        // A retry while the audit still cannot be written fails again, and
        // does not claim to be a repeat.
        let (status, body) = route(&state, "POST", &path, &allow);
        assert_eq!(status, 500, "{body}");
        // The audit can be written now: the retry finishes the decision.
        std::fs::create_dir_all(&missing).unwrap();
        tole_core::storage::JsonlStorage::create(&missing, "srv-audit-retry", None).unwrap();
        // The session is not live here, so after the audit nothing resumes:
        // the answer says so (#979) instead of `resuming: true`.
        let (status, body) = route(&state, "POST", &path, &allow);
        assert_eq!(status, 409, "{body}");
        assert_eq!(body["resuming"], false);
        let audit = std::fs::read_to_string(missing.join("srv-audit-retry.jsonl")).unwrap();
        assert_eq!(
            audit.matches(&format!("approval_{id}")).count(),
            1,
            "{audit}"
        );
        // Done: a further repeat is idempotent and writes nothing.
        let (status, body) = route(&state, "POST", &path, &allow);
        assert_eq!(status, 200, "{body}");
        assert_eq!(body["resuming"], false);
        let audit = std::fs::read_to_string(missing.join("srv-audit-retry.jsonl")).unwrap();
        assert_eq!(
            audit.matches(&format!("approval_{id}")).count(),
            1,
            "{audit}"
        );
    }

    /// #979: an Allow for a session this server no longer hosts, with no
    /// parked cron run, must not answer `resuming: true`; a Deny is still a
    /// plain recorded verdict.
    #[test]
    fn an_allow_for_a_session_that_is_not_live_is_refused_not_promised() {
        let base = crate::test_dir::TestDir::new("tole-serve-evicted-allow");
        let root = base.join("ws");
        let sessions = base.join("sessions");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir_all(&sessions).unwrap();
        let state = test_state(root, sessions.clone());
        tole_core::storage::JsonlStorage::create(&sessions, "srv-gone", None).unwrap();
        let queue = |n: u64| {
            state.approvals.queue_entry(
                "srv-gone",
                "write_file",
                "write file x.txt",
                n,
                sessions.join("srv-gone.jsonl"),
            )
        };
        let id = queue(1);
        let (status, body) = route(
            &state,
            "POST",
            &format!("/approvals/{id}/decision"),
            &json!({"decision": "allow"}).to_string(),
        );
        assert_eq!(status, 409, "{body}");
        assert_eq!(body["resuming"], false);
        assert!(body["error"].as_str().unwrap().contains("not live"));
        // The refused allow left no one-shot behind (F2).
        assert!(!state.approvals.has_pending("srv-gone"));
        let id = queue(2);
        let (status, body) = route(
            &state,
            "POST",
            &format!("/approvals/{id}/decision"),
            &json!({"decision": "deny"}).to_string(),
        );
        assert_eq!(status, 200, "{body}");
    }

    /// cora 0.19 scan MAJOR: REST `POST /sessions` must enforce the same
    /// jail-of-jails as the MCP transport — a cwd outside the server
    /// workspace root is refused BEFORE any session file is created.
    #[test]
    fn rest_session_cwd_outside_workspace_root_is_refused() {
        let base = crate::test_dir::TestDir::new("tole-serve-jail");
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

    /// #947: `plan_mode` on `POST /sessions` removes the mutation tools from
    /// that session's registry (absence, not approval); off by default; a bad
    /// type is a 400; and a process-level plan mode cannot be disabled.
    #[test]
    fn plan_mode_request_filters_the_registry_and_cannot_disable_the_process_flag() {
        let base = crate::test_dir::TestDir::new("tole-serve-plan-mode");
        let root = base.join("ws");
        let sessions = base.join("sessions");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir_all(&sessions).unwrap();
        let mut state = test_state(root.clone(), sessions);
        let cwd = root.to_str().unwrap();
        let open = |state: &State, body: serde_json::Value| {
            let (s, v) = route(state, "POST", "/sessions", &body.to_string());
            (s, v)
        };
        let reg = |state: &State, v: &serde_json::Value| {
            let id = v["sessionId"].as_str().unwrap().to_string();
            let st = lock_sessions(&state.sessions).get(&id).cloned().unwrap();
            st.registry.clone()
        };

        // Default: write tools present, no planMode on the wire.
        let (s, v) = open(&state, json!({"cwd": cwd}));
        assert_eq!(s, 200, "{v}");
        assert!(v.get("planMode").is_none());
        assert!(reg(&state, &v).get("write_file").is_some());

        // Explicit false is the default too.
        let (s, v) = open(&state, json!({"cwd": cwd, "plan_mode": false}));
        assert_eq!(s, 200, "{v}");
        assert!(reg(&state, &v).get("write_file").is_some());

        // On: mutation tools are absent, read tools stay.
        let (s, v) = open(&state, json!({"cwd": cwd, "plan_mode": true}));
        assert_eq!(s, 200, "{v}");
        assert_eq!(v["planMode"], true);
        let r = reg(&state, &v);
        assert!(r.get("read_file").is_some());
        for name in ["write_file", "run_command", "agent_start", "todo_write"] {
            assert!(r.get(name).is_none(), "{name} in plan mode");
        }

        // Bad type, and the resume combination, are 400s.
        for bad in [json!("yes"), json!(1), json!([true])] {
            let (s, _) = open(&state, json!({"cwd": cwd, "plan_mode": bad}));
            assert_eq!(s, 400);
        }
        let (s, _) = open(&state, json!({"resume": "x", "plan_mode": true}));
        assert_eq!(s, 400);

        // A process-level plan mode cannot be disabled by the request.
        state.plan_mode = true;
        let (s, v) = open(&state, json!({"cwd": cwd, "plan_mode": false}));
        assert_eq!(s, 200, "{v}");
        assert_eq!(v["planMode"], true);
        assert!(reg(&state, &v).get("write_file").is_none());
        let (s, v) = open(&state, json!({"cwd": cwd}));
        assert_eq!(s, 200, "{v}");
        assert!(reg(&state, &v).get("write_file").is_none());
    }

    /// #947: a mission request names only its goal and budgets. `verify` (the
    /// owner's command) and every other field is a 400 that starts nothing;
    /// a running mission makes the next one a 409; cancel flips the token of
    /// the running one only; a plan-mode server refuses to start one.
    #[test]
    fn mission_routes_refuse_owner_fields_and_keep_one_mission_at_a_time() {
        let base = crate::test_dir::TestDir::new("tole-serve-mission");
        let root = base.join("ws");
        let sessions = base.join("sessions");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir_all(&sessions).unwrap();
        let mut state = test_state(root, sessions.clone());
        state.mission.verify = Some("true".into());

        for body in [
            json!({"goal": "g", "verify": "rm -rf /"}),
            json!({"goal": "g", "verify": null}),
            json!({"goal": "g", "sandbox": "off"}),
            json!({"goal": "g", "cwd": "/"}),
            json!({"goal": "  "}),
            json!({"goal": "g", "max_steps": 1000000}),
        ] {
            let (s, v) = route(&state, "POST", "/missions", &body.to_string());
            assert_eq!(s, 400, "{body}: {v}");
        }
        let (s, v) = route(
            &state,
            "POST",
            "/missions",
            &json!({"goal": "g", "verify": "x"}).to_string(),
        );
        assert_eq!(s, 400);
        assert!(v["error"].as_str().unwrap().contains("owner"), "{v}");
        assert_eq!(std::fs::read_dir(&sessions).unwrap().count(), 0);

        // A running mission (planted) blocks a second one and takes a cancel.
        let cancel = tole_core::cancel::CancelToken::new();
        state.missions.plant_running("mission-a", cancel.clone());
        let (s, _) = route(&state, "POST", "/missions", r#"{"goal":"g"}"#);
        assert_eq!(s, 409);
        let (s, v) = route(&state, "GET", "/missions/mission-a", "");
        assert_eq!(
            (s, v["status"].clone(), v["running"].clone()),
            (200, json!("running"), json!(true))
        );
        assert!(!cancel.is_cancelled());
        let (s, _) = route(&state, "POST", "/missions/mission-a/cancel", "");
        assert_eq!(s, 202);
        assert!(cancel.is_cancelled());
        let (s, _) = route(&state, "POST", "/missions/mission-b/cancel", "");
        assert_eq!(s, 404);
        let (s, _) = route(&state, "GET", "/missions/mission-b", "");
        assert_eq!(s, 404);

        // Plan mode: no mission.
        let mut ro = test_state(state.workspace_root.clone(), sessions);
        ro.plan_mode = true;
        let (s, _) = route(&ro, "POST", "/missions", r#"{"goal":"g"}"#);
        assert_eq!(s, 409);
    }

    /// Open a serve session in-process and settle one turn in its log, so
    /// there is something to fork. Returns (session id, seq of the answer).
    fn session_with_a_turn(state: &State, cwd: &std::path::Path) -> (String, u64) {
        use tole_core::entry::{EntryType, NewEntry};
        use tole_core::state::{Pc, StateTransition};
        use tole_core::storage::Commit;
        let (status, v) = route(
            state,
            "POST",
            "/sessions",
            &json!({"cwd": cwd.to_str().unwrap()}).to_string(),
        );
        assert_eq!(status, 200, "{v}");
        let id = v["sessionId"].as_str().unwrap().to_string();
        let st = lock_sessions(&state.sessions).get(&id).cloned().unwrap();
        let mut g = st.storage.lock().unwrap();
        let msg = |role: &str, text: &str| {
            NewEntry::root(
                EntryType::new(EntryType::MESSAGE),
                json!({"role": role, "text": text}),
            )
        };
        g.commit(Commit::new().entry(msg("user", "hi"))).unwrap();
        let seq = g.state().seq;
        g.commit(Commit::new().transition(StateTransition::from(seq, Pc::Planning)))
            .unwrap();
        let seq = g.state().seq;
        g.commit(
            Commit::new()
                .entry(msg("assistant", "hello"))
                .transition(StateTransition::from(seq, Pc::Final)),
        )
        .unwrap();
        let answer = g
            .entries()
            .iter()
            .rev()
            .find(|e| e.payload["role"] == "assistant")
            .unwrap()
            .seq;
        (id, answer)
    }

    /// #947: fork and tree over serve: success shape, the family tree, 404,
    /// 409 while busy, and 4xx for an `at` that is not a settled entry.
    #[test]
    fn fork_and_tree_routes_answer_success_404_409_and_bad_at() {
        let base = crate::test_dir::TestDir::new("tole-serve-fork");
        let root = base.join("ws");
        let sessions = base.join("sessions");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir_all(&sessions).unwrap();
        let state = test_state(root.clone(), sessions.clone());
        let (id, answer) = session_with_a_turn(&state, &root);

        // Unknown session: 404 on both routes.
        let (s, _) = route(&state, "POST", "/sessions/nope/fork", "{}");
        assert_eq!(s, 404);
        let (s, _) = route(&state, "GET", "/sessions/nope/tree", "");
        assert_eq!(s, 404);

        // A user message is not a settled point; an unknown seq is no entry.
        let user_seq = {
            let st = lock_sessions(&state.sessions).get(&id).cloned().unwrap();
            let g = st.storage.lock().unwrap();
            g.entries()
                .iter()
                .find(|e| e.payload["role"] == "user")
                .unwrap()
                .seq
        };
        let path = format!("/sessions/{id}/fork");
        for bad in [
            json!({"at": user_seq}),
            json!({"at": 99999}),
            json!({"at": "x"}),
        ] {
            let (s, v) = route(&state, "POST", &path, &bad.to_string());
            assert_eq!(s, 400, "{bad}: {v}");
        }
        let (s, _) = route(&state, "POST", &path, "not json");
        assert_eq!(s, 400);
        let logs = || {
            std::fs::read_dir(&sessions)
                .unwrap()
                .flatten()
                .filter(|e| e.file_name().to_string_lossy().ends_with(".jsonl"))
                .count()
        };
        assert_eq!(logs(), 1, "a refused fork leaves no log behind");

        // 409 while a turn runs.
        let st = lock_sessions(&state.sessions).get(&id).cloned().unwrap();
        *st.busy.lock().unwrap() = true;
        let (s, _) = route(&state, "POST", &path, "{}");
        assert_eq!(s, 409);
        *st.busy.lock().unwrap() = false;

        // Success at an explicit seq, and again at the default.
        let (s, v) = route(&state, "POST", &path, &json!({"at": answer}).to_string());
        assert_eq!(s, 200, "{v}");
        assert_eq!(v["forkedFrom"], json!(id));
        assert_eq!(v["seq"], json!(answer));
        let fork_id = v["id"].as_str().unwrap().to_string();
        assert!(sessions.join(format!("{fork_id}.jsonl")).exists());
        let (s, v) = route(&state, "POST", &path, "");
        assert_eq!(s, 200, "{v}");
        let fork2 = v["id"].as_str().unwrap().to_string();
        assert_ne!(fork2, fork_id);

        // The tree: the source at depth 0, both forks below it.
        let (s, v) = route(&state, "GET", &format!("/sessions/{id}/tree"), "");
        assert_eq!(s, 200, "{v}");
        let rows = v["tree"].as_array().unwrap();
        assert_eq!(rows.len(), 3, "{v}");
        assert_eq!(rows[0]["id"], json!(id));
        assert_eq!(rows[0]["depth"], 0);
        assert_eq!(rows[0]["current"], true);
        assert!(rows[0]["parent"].is_null());
        let child = rows.iter().find(|r| r["id"] == json!(fork_id)).unwrap();
        assert_eq!(child["parent"], json!(id));
        assert_eq!(child["depth"], 1);
        assert_eq!(child["current"], false);
        assert_eq!(child["turns"], 1);
        assert!(child["at"].is_string());
        // The tree is a file read: it answers while a turn runs.
        *st.busy.lock().unwrap() = true;
        let (s, _) = route(&state, "GET", &format!("/sessions/{id}/tree"), "");
        assert_eq!(s, 200);
        *st.busy.lock().unwrap() = false;
    }

    /// Open a serve session in-process; returns its id.
    fn open_session(state: &State, cwd: &std::path::Path) -> String {
        let (status, v) = route(
            state,
            "POST",
            "/sessions",
            &json!({"cwd": cwd.to_str().unwrap()}).to_string(),
        );
        assert_eq!(status, 200, "{v}");
        v["sessionId"].as_str().unwrap().to_string()
    }

    /// #947: compact and name over serve write what the TUI writes (read
    /// back through the session's own readers), 404 an unknown session, 409
    /// while a turn runs, and 400 a bad name.
    #[test]
    fn compact_and_name_routes_write_the_tui_registers_404_409_and_bad_name() {
        let base = crate::test_dir::TestDir::new("tole-serve-compact-name");
        let root = base.join("ws");
        let sessions = base.join("sessions");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir_all(&sessions).unwrap();
        let state = test_state(root.clone(), sessions);
        let id = open_session(&state, &root);
        let st = lock_sessions(&state.sessions).get(&id).cloned().unwrap();
        let compact = format!("/sessions/{id}/compact");
        let name = format!("/sessions/{id}/name");
        let named = json!({"name": "nightly"}).to_string();

        for (path, body) in [
            ("/sessions/nope/compact", ""),
            ("/sessions/nope/name", named.as_str()),
        ] {
            let (s, _) = route(&state, "POST", path, body);
            assert_eq!(s, 404, "{path}");
        }

        // 409 while a turn runs, and nothing is written.
        *st.busy.lock().unwrap() = true;
        let (s, _) = route(&state, "POST", &compact, "");
        assert_eq!(s, 409);
        let (s, _) = route(&state, "POST", &name, &named);
        assert_eq!(s, 409);
        *st.busy.lock().unwrap() = false;
        {
            let g = st.storage.lock().unwrap();
            assert_eq!(g.session_name(), None);
            assert!(!tole_core::compaction::overflow_pending(&*g));
        }
        // The turn lock itself held (a turn between the check and the write).
        {
            let _held = st.storage.lock().unwrap();
            let (s, _) = route(&state, "POST", &compact, "");
            assert_eq!(s, 409);
        }

        // Bad names are a 400 and write nothing.
        for bad in [
            "not json".to_string(),
            json!({}).to_string(),
            json!({"name": 5}).to_string(),
            json!({"name": "  "}).to_string(),
            json!({"name": "a\u{1b}b"}).to_string(),
            json!({"name": "x".repeat(121)}).to_string(),
        ] {
            let (s, v) = route(&state, "POST", &name, &bad);
            assert_eq!(s, 400, "{bad}: {v}");
        }

        // Success: the response shape and what the TUI would have written.
        let (s, v) = route(
            &state,
            "POST",
            &name,
            &json!({"name": " nightly "}).to_string(),
        );
        assert_eq!(s, 200, "{v}");
        assert_eq!(v, json!({"id": id, "name": "nightly"}));
        let (s, v) = route(&state, "POST", &compact, "");
        assert_eq!(s, 200, "{v}");
        assert_eq!(v, json!({"id": id, "compaction": "requested"}));
        let g = st.storage.lock().unwrap();
        assert_eq!(g.session_name(), Some("nightly"));
        assert!(tole_core::compaction::overflow_pending(&*g));
    }

    /// #574: the model list is the owner's, `model` and `effort` write the
    /// registers the TUI writes (read back through the session's readers),
    /// and 404 / 409 / 400 hold, including a model outside the list.
    #[test]
    fn model_and_effort_routes_write_the_tui_registers_and_obey_the_allow_list() {
        let base = crate::test_dir::TestDir::new("tole-serve-model-effort");
        let root = base.join("ws");
        let sessions = base.join("sessions");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir_all(&sessions).unwrap();
        let mut state = test_state(root.clone(), sessions);
        state.models = models::ModelCatalog::new(
            Some(" gpt-a, gpt-b ,gpt-a,,bad\u{7}id"),
            Some("gpt-default".into()),
        );
        let id = open_session(&state, &root);
        let st = lock_sessions(&state.sessions).get(&id).cloned().unwrap();
        let model = format!("/sessions/{id}/model");
        let effort = format!("/sessions/{id}/effort");
        let pick = |m: &str| json!({"model": m}).to_string();

        // The list: the default first, then the owner's allow list.
        let (s, v) = route(&state, "GET", "/models", "");
        assert_eq!(s, 200);
        assert_eq!(
            v,
            json!({"models": ["gpt-default", "gpt-a", "gpt-b"], "default": "gpt-default"})
        );

        // 404 on an unknown session.
        let (s, _) = route(&state, "POST", "/sessions/nope/model", &pick("gpt-a"));
        assert_eq!(s, 404);
        let (s, _) = route(
            &state,
            "POST",
            "/sessions/nope/effort",
            r#"{"effort":"low"}"#,
        );
        assert_eq!(s, 404);

        // 409 while a turn runs, and nothing is written.
        *st.busy.lock().unwrap() = true;
        assert_eq!(route(&state, "POST", &model, &pick("gpt-a")).0, 409);
        assert_eq!(route(&state, "POST", &effort, r#"{"effort":"low"}"#).0, 409);
        *st.busy.lock().unwrap() = false;
        {
            let _held = st.storage.lock().unwrap();
            assert_eq!(route(&state, "POST", &model, &pick("gpt-a")).0, 409);
            assert_eq!(route(&state, "POST", &effort, r#"{"effort":"low"}"#).0, 409);
        }
        {
            let g = st.storage.lock().unwrap();
            assert_eq!(tole_cli::turn_runner::session_model(&*g, "dflt"), "dflt");
            assert_eq!(tole_cli::turn_runner::session_effort(&*g, None), None);
        }

        // 400: bad bodies, a model outside the list (also a provider URL,
        // another case, a prefix of an allowed id) and bad levels.
        for bad in [
            "not json".to_string(),
            json!({}).to_string(),
            json!({"model": 5}).to_string(),
            json!({"model": "  "}).to_string(),
            pick("gpt-c"),
            pick("GPT-A"),
            pick("gpt"),
            pick("https://evil.example/v1"),
            json!({"model": "gpt-a", "base_url": "http://evil", "api_key": "k"})
                .to_string()
                .replace("gpt-a", "not-allowed"),
        ] {
            let (s, v) = route(&state, "POST", &model, &bad);
            assert_eq!(s, 400, "{bad}: {v}");
        }
        for bad in [
            "not json".to_string(),
            json!({}).to_string(),
            json!({"effort": "ultra"}).to_string(),
            json!({"effort": ""}).to_string(),
            json!({"effort": 3}).to_string(),
        ] {
            let (s, v) = route(&state, "POST", &effort, &bad);
            assert_eq!(s, 400, "{bad}: {v}");
        }
        {
            let g = st.storage.lock().unwrap();
            assert_eq!(tole_cli::turn_runner::session_model(&*g, "dflt"), "dflt");
            assert_eq!(tole_cli::turn_runner::session_effort(&*g, None), None);
        }

        // Success: the response shape and the registers the TUI reads.
        let (s, v) = route(&state, "POST", &model, &pick(" gpt-b "));
        assert_eq!(s, 200, "{v}");
        assert_eq!(v, json!({"id": id, "model": "gpt-b"}));
        let (s, v) = route(&state, "POST", &effort, r#"{"effort":" HIGH "}"#);
        assert_eq!(s, 200, "{v}");
        assert_eq!(v, json!({"id": id, "effort": "high"}));
        {
            let g = st.storage.lock().unwrap();
            assert_eq!(g.get_register("fact", "model"), Some(&json!("gpt-b")));
            assert_eq!(
                g.get_register("fact", "reasoning_effort"),
                Some(&json!("high"))
            );
            assert_eq!(tole_cli::turn_runner::session_model(&*g, "dflt"), "gpt-b");
            assert_eq!(
                tole_cli::turn_runner::session_effort(&*g, Some("low")),
                Some("high".into())
            );
        }
        // The default is always allowed (switch back); null and "unset" clear.
        assert_eq!(route(&state, "POST", &model, &pick("gpt-default")).0, 200);
        let (s, v) = route(&state, "POST", &effort, r#"{"effort":null}"#);
        assert_eq!((s, v["effort"].clone()), (200, json!(null)));
        route(&state, "POST", &effort, r#"{"effort":"low"}"#);
        assert_eq!(
            route(&state, "POST", &effort, r#"{"effort":"unset"}"#).0,
            200
        );
        let g = st.storage.lock().unwrap();
        assert_eq!(
            tole_cli::turn_runner::session_model(&*g, "dflt"),
            "gpt-default"
        );
        assert_eq!(g.get_register("fact", "reasoning_effort"), None);
    }

    /// #574: with no `TOLE_MODELS` the default is the only model a client
    /// can pick, and with no default either the list is empty.
    #[test]
    fn without_an_owner_list_only_the_default_can_be_picked() {
        let base = crate::test_dir::TestDir::new("tole-serve-model-default");
        let root = base.join("ws");
        let sessions = base.join("sessions");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir_all(&sessions).unwrap();
        let mut state = test_state(root.clone(), sessions);
        state.models = models::ModelCatalog::new(None, Some("only".into()));
        let id = open_session(&state, &root);
        let model = format!("/sessions/{id}/model");
        assert_eq!(
            route(&state, "GET", "/models", "").1["models"],
            json!(["only"])
        );
        assert_eq!(route(&state, "POST", &model, r#"{"model":"other"}"#).0, 400);
        assert_eq!(route(&state, "POST", &model, r#"{"model":"only"}"#).0, 200);
        state.models = models::ModelCatalog::new(None, None);
        let (s, v) = route(&state, "GET", "/models", "");
        assert_eq!((s, v), (200, json!({"models": [], "default": null})));
        assert_eq!(route(&state, "POST", &model, r#"{"model":"only"}"#).0, 400);
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
        let server_side = Conn::Tcp(server_side);
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
