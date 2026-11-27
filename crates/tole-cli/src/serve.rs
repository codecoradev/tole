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

use anyhow::Result;
use serde_json::json;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use approval_audit::write_approval_audit;
use events::serve_events;
use hooks::handle_hook;
use images::{image_error, read_image, respond_image, upload_image};
use live_sessions::{open_serve_session, resume_session, serve_storage_dir, warm_status};
use request::{read_request, ReadError};
use response::{respond, respond_unauthorized, respond_with};
use session_routes::{compact_route, fork_route, name_route, tree_route, with_idle_session};
use tole_cli::serve_support::profiles::ProfileSource;
use tole_cli::serve_support::shutdown;
use tole_cli::serve_support::transport::Conn;
use tole_cli::session_host::{lock_sessions, run_session_turn_with_end, SharedSessions};
use tole_core::memory::MemoryConfig;
use tole_core::storage::Storage;

mod approval_audit;
mod cron;
mod discussions;
mod events;
mod hooks;
mod images;
mod live_sessions;
pub mod missions;
mod models;
pub mod past_sessions;
mod pct;
mod request;
mod response;
mod run;
mod session_routes;
mod state_views;
mod token;

pub use run::run_serve;
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

/// `/sessions/{id}/events[?query]` -> `(id, query)`.
fn events_target(path: &str) -> Option<(&str, &str)> {
    let (path, query) = path.split_once('?').unwrap_or((path, ""));
    let id = path.strip_prefix("/sessions/")?.strip_suffix("/events")?;
    (!id.is_empty() && !id.contains('/')).then_some((id, query))
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

#[cfg(test)]
mod tests {
    mod approvals;
    mod connection;
    mod missions;
    mod models;
    mod sessions;
    mod support;
}
