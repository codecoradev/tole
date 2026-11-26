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
//! SIGTERM and SIGINT start a graceful shutdown (#1014; see
//! `tole_cli::serve_support::shutdown`): new requests get 503, running turns are
//! detached through the session host's detach path, the cron ticker and the
//! webhook senders stop, and the process exits 0 once nothing is left, or
//! non-zero when `--shutdown-grace` (default 20 s) runs out or a second signal
//! arrives.
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
use std::io::Write;
use std::net::TcpListener;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use request::{read_request, ReadError, Request};
use response::{respond, respond_unauthorized, respond_with};
use tole_cli::serve_support::profiles::{self, ProfileSource, SessionProfile};
use tole_cli::serve_support::shutdown;
use tole_cli::serve_support::transport::{self, Conn, Listener};
use tole_cli::session_host::{
    lock_sessions, open_session_with, run_session_turn_with_end, SessionOverrides, Sessions,
    SharedSessions,
};
use tole_core::memory::MemoryConfig;
use tole_core::storage::Storage;

mod cron;
mod discussions;
pub mod missions;
mod models;
pub mod past_sessions;
mod pct;
mod request;
mod response;
mod state_views;
mod token;

pub use token::{hash_command, serve_token, serve_token_optional};

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
    /// How long a SIGTERM/SIGINT drain waits for running turns before it exits
    /// anyway (#1014).
    pub shutdown_grace: Duration,
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
    /// The discussions this process runs for its owner (#1042).
    discussions: Arc<tole_cli::discuss::owner::Owner>,
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
    turn: &Result<tole_cli::session_host::TurnResult, tole_cli::session_host::TurnError>,
) {
    let (end, approval_tool, queue_mark) = match turn {
        Ok(r) => (r.turn_end, r.approval_tool.as_deref(), r.queue_mark),
        Err(e) => (e.end, None, None),
    };
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
pub fn run_serve(cfg: ServeConfig) -> Result<()> {
    // No human answers a serve session's start-up: an `optional` sandbox
    // counts as `required` here (#601).
    tole_core::sandbox::mark_unattended();
    // Before the listener is reported ready: a SIGTERM that arrives right after
    // the "listening" line must already be counted (#1014).
    shutdown::install().context("installing the SIGTERM and SIGINT handlers")?;
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
        discussions: Arc::default(),
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
    // Every helper thread (sender, ticker, event workers) ends when `stop` is
    // set at shutdown, and is waited for within the grace period (#1014).
    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let mut helpers: Vec<std::thread::JoinHandle<()>> = Vec::new();
    if !webhook_out.is_empty() {
        let dir = tole_cli::cron::default_dir()?;
        eprintln!(
            "tole serve: webhook sender for {} destination(s)",
            webhook_out.len()
        );
        helpers.push(tole_cli::cron::webhook::spawn_sender(
            dir,
            Arc::clone(&webhook_out),
            Arc::clone(&stop),
        ));
    }
    // Approval events (#911): when a destination lists them, the approval queue
    // tells a sender thread about each request and each resolution, and a timer
    // expires what nobody answers. Absent, the approval paths are unchanged.
    // The same sender also carries `turn_finished` (#962); its notifier is kept
    // for the turn routes.
    let spawned = tole_cli::webhook_out::approval_sender::spawn(
        Arc::clone(&webhook_out),
        Arc::clone(&state.approvals),
        Arc::clone(&stop),
    );
    helpers.extend(spawned.threads);
    if let Some(turns) = spawned.turns {
        let _ = state.turn_events.set(turns);
    }
    if cfg.cron {
        let dir = tole_cli::cron::default_dir()?;
        eprintln!("tole serve: cron ticker on {}", dir.display());
        let queue = Arc::clone(&state.approvals);
        let stop = Arc::clone(&stop);
        helpers.push(std::thread::spawn(move || {
            tole_cli::cron::serve_ticker(&dir, queue, &stop)
        }));
    }
    use std::sync::atomic::Ordering;
    let mut draining: Option<shutdown::Countdown> = None;
    let ending = loop {
        if draining.is_none() && shutdown::requested() {
            // Stop everything that could start new work; the loop below keeps
            // accepting only to answer 503 (see `handle_conn`), so a client is
            // never left waiting on a queue nobody serves, and the socket and its
            // lock stay held until the very end.
            stop.store(true, Ordering::Relaxed);
            let running = tole_cli::session_host::detach_running_turns(&state.sessions)
                + state.missions.cancel_running();
            eprintln!(
                "tole serve: shutting down (grace {}s): {running} running turn(s) asked to detach",
                cfg.shutdown_grace.as_secs()
            );
            draining = Some(shutdown::Countdown::start(cfg.shutdown_grace));
        }
        if let Some(countdown) = &draining {
            // Again each pass: a request read just before the flag may claim its
            // session after the first sweep.
            let turns = tole_cli::session_host::detach_running_turns(&state.sessions);
            let missions = state.missions.cancel_running();
            let drained = turns + missions == 0
                && helpers.iter().all(|h| h.is_finished())
                && state.live_connections.load(Ordering::Relaxed) == 0;
            if let Some(ending) = countdown.check(drained) {
                break ending;
            }
        }
        let stream = match listener.accept_within(shutdown::POLL) {
            Ok(Some(stream)) => stream,
            Ok(None) => continue,
            Err(e) => {
                // A transient accept error (EMFILE, ECONNABORTED, ...) must not
                // end the server; the pause keeps resource exhaustion from
                // spinning.
                eprintln!("tole serve: accept failed (continuing): {e}");
                std::thread::sleep(shutdown::POLL);
                continue;
            }
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
    };
    // Leaving the function drops the listener: a local socket's file is removed
    // and its `<socket>.lock` is released, the supervisor's proof that this
    // serve is gone. Session writer locks go with the process.
    let left = || {
        let turns = lock_sessions(&state.sessions)
            .iter()
            .filter(|(_, st)| *st.busy.lock().expect("busy lock"))
            .count();
        let alive = helpers.iter().filter(|h| !h.is_finished()).count();
        format!("{turns} running turn(s) and {alive} helper thread(s)")
    };
    if ending == shutdown::Ending::Clean {
        eprintln!("tole serve: drained, exiting");
    }
    ending.into_result("tole serve", cfg.shutdown_grace, left)
}

// ---------------------------------------------------------------------------
// Connection handling
// ---------------------------------------------------------------------------

/// The route of a `/hooks/<route>` path.
fn hook_target(path: &str) -> Option<&str> {
    path.strip_prefix("/hooks/")
        .map(|r| r.split('?').next().unwrap_or(r))
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

    // Shutting down (#1014): nothing new starts, and a client is told so instead
    // of being left to a timeout. Not behind auth: it says nothing a refused
    // connection would not.
    if shutdown::requested() {
        respond_with(
            &mut stream,
            503,
            &json!({"error": "tole serve is shutting down"}),
            "Retry-After: 1\r\n",
        );
        return;
    }

    // Auth: bearer token on everything except /health (a liveness probe that
    // reports only the protocol and binary version, see the threat model).
    // Failed auths are rate-limited
    // per source IP (CodeCora scan-3 Wave-2: brute-force hardening).

    // Rate-limit: refuse when this IP has too many recent auth failures. On the
    // local socket every peer shares one bucket, so the unauthenticated liveness
    // probe is exempt there: a client with a stale token must not be able to make
    // a running serve look dead to the one that attaches (TCP is unchanged).
    let socket_probe = stream.is_local_socket() && req.method == "GET" && req.path == "/health";
    if limited && !socket_probe {
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
            match pct::id(id) {
                Ok(id) => serve_events(stream, &state, &id, query, req.last_event_id.as_deref()),
                Err((status, payload)) => respond(&mut stream, status, &payload),
            }
            return;
        }
    }

    // Images (#558): a binary body in, binary bytes out.
    if req.method == "POST" {
        if let Some(id) = upload_target(&req.path) {
            let (status, payload) = match pct::id(id) {
                Ok(id) => upload_image(&state, &id, &req.body),
                Err(refused) => refused,
            };
            respond(&mut stream, status, &payload);
            return;
        }
    }
    if req.method == "GET" {
        if let Some((id, hash)) = image_target(&req.path) {
            let read = match (pct::id(id), pct::id(hash)) {
                (Ok(id), Ok(hash)) => read_image(&state, &id, &hash),
                (Err(refused), _) | (_, Err(refused)) => Err(refused),
            };
            match read {
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
        // Shutting down (#1014): end the stream once the session's turn has
        // settled, entries included, so the drain never waits on an idle viewer.
        // The client reconnects with `after` (to the next serve).
        if shutdown::requested() && live_busy(state) != Some(true) {
            let _ = tail
                .poll()
                .map(|rest| rest.iter().all(|e| write_all(&mut stream, &entry_frame(e))));
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

/// The id of a route shaped `{prefix}{id}{suffix}`, or `None` when the path is
/// not of that shape. The id may be empty (`/discussions//resume`) and the
/// caller refuses it. A path that is only the suffix after the collection
/// (`/discussions/resume`) starts with the prefix and ends with the suffix at
/// once, and slicing it by the two lengths made a reversed range and a panic,
/// so the prefix is stripped first and the suffix from what is left.
fn id_between<'a>(path: &'a str, prefix: &str, suffix: &str) -> Option<&'a str> {
    path.strip_prefix(prefix)?.strip_suffix(suffix)
}

/// Runs `f` with the percent-decoded id of a route matched by [`id_between`]
/// (the match is on the raw path, the id is decoded after it: see `pct`), or
/// answers the `400` of a value that does not decode.
fn with_id(
    raw: Option<&str>,
    f: impl FnOnce(&str) -> (u16, serde_json::Value),
) -> (u16, serde_json::Value) {
    match pct::id(raw.unwrap_or_default()) {
        Ok(id) => f(&id),
        Err(refused) => refused,
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
            with_id(Some(&p["/missions/".len()..]), |id| {
                missions::status(state, id)
            })
        }
        ("POST", p) if id_between(p, "/missions/", "/cancel").is_some() => {
            with_id(id_between(p, "/missions/", "/cancel"), |id| {
                missions::cancel(state, id)
            })
        }
        // Discussions (#1042): the owner's `tole discuss`, read and dry run.
        ("GET", "/discussions") => discussions::list(state),
        ("POST", "/discussions/estimate") => discussions::estimate(state, body),
        ("POST", "/discussions") => discussions::start(state, body),
        ("POST", p) if id_between(p, "/discussions/", "/resume").is_some() => {
            with_id(id_between(p, "/discussions/", "/resume"), |id| {
                discussions::resume(state, id)
            })
        }
        ("POST", p) if id_between(p, "/discussions/", "/stop").is_some() => {
            with_id(id_between(p, "/discussions/", "/stop"), |id| {
                discussions::stop(state, id)
            })
        }
        ("GET", p)
            if p.starts_with("/discussions/") && !p["/discussions/".len()..].contains('/') =>
        {
            with_id(Some(&p["/discussions/".len()..]), |id| {
                discussions::show(state, id)
            })
        }
        // The owner's profiles, skills and connected tools (#1042): read-only.
        ("GET", "/profiles") => state_views::profiles(state),
        ("GET", "/skills") => state_views::skills(state),
        ("GET", "/tools") => state_views::tools(state),
        // Cron jobs (#1042): read, pause, and resume what the owner reviewed.
        ("GET", "/cron/jobs") => cron::list(),
        ("POST", p) if id_between(p, "/cron/jobs/", "/pause").is_some() => {
            with_id(id_between(p, "/cron/jobs/", "/pause"), |id| {
                cron::set_paused(state, id, true)
            })
        }
        ("POST", p) if id_between(p, "/cron/jobs/", "/resume").is_some() => {
            with_id(id_between(p, "/cron/jobs/", "/resume"), |id| {
                cron::set_paused(state, id, false)
            })
        }
        // Cron run results (#579, design §9): the pull delivery target.
        ("GET", p) if p == "/cron/runs" || p.starts_with("/cron/runs?") => {
            let query = p.split_once('?').map_or("", |(_, q)| q);
            let limit = match pct::raw_param(query, "limit").map(str::parse::<usize>) {
                None => None,
                Some(Ok(n)) => Some(n),
                Some(Err(_)) => return (400, json!({"error": "limit must be a number"})),
            };
            let job = match pct::param(query, "job") {
                Ok(job) => job,
                Err(refused) => return refused,
            };
            let runs = tole_cli::cron::default_dir()
                .and_then(|dir| tole_cli::cron::runs_json(&dir, job.as_deref(), limit));
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
            let session = match pct::param(p.split_once('?').map_or("", |(_, q)| q), "session") {
                Ok(session) => session.map(|s| s.into_owned()),
                Err(refused) => return refused,
            };
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
            let id = match pct::id(
                p.trim_start_matches("/approvals/")
                    .trim_end_matches("/decision"),
            ) {
                Ok(id) => id.into_owned(),
                Err(refused) => return refused,
            };
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
                        let turn = tole_cli::session_host::resume_session_turn_with_end(
                            Arc::clone(&sessions),
                            &sid,
                            Some(&queue),
                        );
                        if let Err(e) = &turn {
                            eprintln!("tole serve: approval resume failed: {}", e.message);
                        }
                        announce_turn(turn_notifier.as_ref(), &queue, &sid, &turn);
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
            // The id is decoded after the raw path was split (see `pct`).
            let id = match pct::id(id) {
                Ok(id) => id,
                Err(refused) => return refused,
            };
            let id: &str = &id;
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
                        // Not reachable (the query parser reads no file),
                        // answered like the page read below.
                        Err(tole_cli::protocol::EntriesError::Missing) => {
                            return (404, json!({"error": "session transcript not found"}))
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
                        // The file is gone while the session is still registered
                        // (removed from outside, #915): a defined 404, not a raw
                        // OS error as a 500.
                        Err(tole_cli::protocol::EntriesError::Missing) => {
                            (404, json!({"error": "session transcript not found"}))
                        }
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
                    announce_turn(state.turn_events.get(), &state.approvals, id, &turn);
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
                        Err(e) => tole_cli::session::interrupted::prompt_error(e.message),
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
    mod approvals;
    mod connection;
    mod missions;
    mod models;
    mod sessions;
    mod support;
}
