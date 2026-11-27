//! `run_serve`: start `tole serve`, accept connections until a signal, drain and exit.

use super::live_sessions::sessions_keeping_pending;
use super::{handle_conn, models, ServeConfig, State, SERVE_IO_TIMEOUT};
use anyhow::{Context, Result};
use std::net::TcpListener;
use std::sync::{Arc, Mutex};
use tole_cli::serve_support::shutdown;
use tole_cli::serve_support::transport::{self, Listener};
use tole_cli::session_host::lock_sessions;

/// Max concurrently open connections: each pinned thread holds ~8 KiB
/// stack + a socket; capping bounds thread/socket exhaustion (CodeCora
/// scan-3 Wave-2 hardening).
const MAX_CONNECTIONS: usize = 32;

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
