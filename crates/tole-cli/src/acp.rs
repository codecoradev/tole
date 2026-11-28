//! D2 (issue #95): `tole acp` — an **Agent Client Protocol** host.
//!
//! Editors and ACP-capable clients (Zed et al.) launch `tole acp` and
//! drive a durable tole session over line-delimited JSON-RPC on
//! stdin/stderr:
//!
//! - `initialize` → protocol + capability handshake
//! - `session/new` / `session/load` → a durable JSONL session (workspace
//!   jail rooted at the client-provided cwd)
//! - `session/prompt` → one full tole turn; the final answer is delivered
//!   as an `agent_message_chunk` update before the response (v1 has no
//!   intra-turn streaming — the turn loop is synchronous; the response is
//!   correct, just not incremental)
//! - Write/Destructive tool calls surface as
//!   `session/request_permission` **requests to the editor** — the human
//!   in the editor is the approver, which is exactly tole's gate with a
//!   different face. Destructive tools therefore CAN be exposed here
//!   (unlike MCP server mode): consent is structurally a human decision.
//!
//! Wire hygiene: protocol messages go to stdout; all diagnostics go to
//! stderr. No new dependencies — the JSON-RPC framing is hand-rolled
//! line-delimited JSON (serde_json only).
//!
//! The session machinery lives in [`tole_cli::session_host`], shared with `tole serve`.

mod bridge;
mod config_options;
#[cfg(test)]
mod tests;
mod wire;

use anyhow::Result;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::io::{BufRead, Write};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};

use bridge::{AcpObserver, AcpPrompt};
use config_options::{
    config_options_for, resolve_model_list, session_model_override, set_config_option, ProbeCache,
    SessionApprovalState,
};
use tole_cli::approver::InteractiveApprover;
use tole_cli::session::problem::prompt_result;
use tole_cli::session_host::{
    lock_sessions, new_session_id, open_session, run_session_turn_detailed, validate_session_id,
    AdmitError, Sessions,
};
use wire::{prompt_content, reply, reply_error, Conn};

const ACP_PROTOCOL_VERSION: u32 = 1;

/// Run the ACP agent over stdin/stdout. Blocks until the client closes
/// stdin.
pub fn run_acp(
    allow_patterns: &[String],
    auto_write: bool,
    _workspace_default: Option<&String>,
    plan_mode: bool,
    memory: Option<tole_core::memory::MemoryConfig>,
    sessions_dir: Option<std::path::PathBuf>,
    turnend: Vec<String>,
) -> Result<()> {
    let (line_tx, line_rx) = mpsc::channel::<String>();
    let conn = Conn::new(line_tx.clone());

    // Writer thread: stdout belongs to the protocol.
    std::thread::spawn(move || {
        let stdout = std::io::stdout();
        let mut out = stdout.lock();
        for line in line_rx {
            if out.write_all(line.as_bytes()).is_err() {
                break;
            }
            let _ = out.flush();
        }
    });

    // The session map lives for the WHOLE server lifetime and is shared
    // with prompt threads (Arc clone per prompt). A turn holds only its
    // own per-session storage lock, so this reader loop stays live for
    // permission routing while prompts run.
    let sessions: tole_cli::session_host::SharedSessions =
        Arc::new(Mutex::new(Sessions::default()));
    // Per-session shared approval state (issue #176 PR 2): addressable
    // by session id so set_config_option can flip the session's
    // auto-write while the approver owns the same handles.
    let approval_states: Arc<Mutex<HashMap<String, SessionApprovalState>>> =
        Arc::new(Mutex::new(HashMap::new()));
    // Advertised model list (issues #176/#195/#319): `TOLE_MODELS` wins
    // as-is; otherwise the provider's `GET /models` is probed lazily on
    // first use (agent spawn stays instant). A success is cached for the
    // process lifetime; a failure/timeout only for PROBE_FAIL_TTL, so a
    // hung gateway neither stalls every call nor is pinned forever. No
    // provider config -> nothing to probe.
    let env_models_raw = std::env::var("TOLE_MODELS").ok();
    let probe_cfg = tole_cli::config_apply::provider_config();
    /// Bounded wait for the lazy /models probe (issues #257/#319). It runs
    /// inline on the ACP reader thread, so 5s (a healthy gateway answers well
    /// under 1s) caps the stall; a failure is remembered for PROBE_FAIL_TTL,
    /// so a hung gateway costs at most one 5s wait per 30s.
    const PROBE_WAIT: std::time::Duration = std::time::Duration::from_secs(5);

    let models_cache: Mutex<ProbeCache> = Mutex::new(ProbeCache::default());
    // Callers must hold no sessions/approval lock while calling this: the
    // probe can block for up to PROBE_WAIT (issue #319).
    let model_list = || {
        resolve_model_list(env_models_raw.as_deref(), || {
            let Some(cfg) = probe_cfg.as_ref() else {
                return Vec::new();
            };
            let mut cache = models_cache.lock().unwrap_or_else(|p| p.into_inner());
            cache.get(std::time::Instant::now(), || {
                let base = cfg.base_url.clone();
                let key = cfg.api_key.clone();
                // Detached probe + bounded wait (join() would block the
                // reader thread for the whole gateway timeout).
                let (tx, rx) = std::sync::mpsc::channel();
                std::thread::spawn(move || {
                    let _ = tx.send(tole_core::openai::fetch_model_ids(&base, &key));
                });
                match rx.recv_timeout(PROBE_WAIT) {
                    Ok(Ok(list)) if !list.is_empty() => Some(list),
                    Ok(Ok(_)) => {
                        eprintln!(
                            "tole acp: provider /models returned no ids — set \
                             TOLE_MODELS to advertise a model picker"
                        );
                        None
                    }
                    Ok(Err(e)) => {
                        eprintln!("tole acp: {e} — set TOLE_MODELS to advertise a model picker");
                        None
                    }
                    Err(_) => {
                        eprintln!(
                            "tole acp: provider /models probe did not answer within \
                             {PROBE_WAIT:?} — set TOLE_MODELS to advertise a model picker"
                        );
                        None
                    }
                }
            })
        })
    };
    let env_model = std::env::var("TOLE_MODEL")
        .ok()
        .or_else(|| std::env::var("OPENAI_MODEL").ok())
        .filter(|m| !m.trim().is_empty())
        // Project config (#208) only when the env names no model.
        .or_else(tole_cli::config_apply::config_model);
    let stdin = std::io::stdin();
    for line in stdin.lock().lines() {
        let Ok(line) = line else { break };
        if line.trim().is_empty() {
            continue;
        }
        let Ok(msg) = serde_json::from_str::<Value>(&line) else {
            continue; // non-JSON noise on the protocol channel: ignore
        };
        let id = msg.get("id").cloned();
        let Some(method) = msg
            .get("method")
            .and_then(Value::as_str)
            .map(str::to_string)
        else {
            // A response to one of OUR requests (permission). Client
            // ERROR replies route as well — an errored permission must
            // fail closed immediately, not hang for the full timeout
            // (CodeCora scan 2026-09-28).
            if let Some(id) = msg.get("id").and_then(Value::as_u64) {
                conn.route_reply(id, &msg);
            }
            continue;
        };
        let params = msg.get("params").cloned().unwrap_or(json!({}));

        match method.as_str() {
            "initialize" => {
                reply(
                    &conn,
                    id,
                    json!({
                        "protocolVersion": ACP_PROTOCOL_VERSION,
                        "agentCapabilities": {
                            "loadSession": true,
                            // #559: `image` blocks are accepted (spilled and
                            // referenced like a CLI `data:` URL) whenever
                            // there is a spill dir; audio and embedded
                            // resources stay off. ACP spec (checked
                            // 2026-11-11): `image` defaults to false and an
                            // image block needs `data` (base64), `mimeType`
                            // and may carry a `uri`.
                            "promptCapabilities": {
                                "image": tole_cli::turn_runner::inline_images_supported(),
                                "audio": false,
                                "embeddedContext": false,
                            },
                        },
                        "agentInfo": {
                            "name": "tole",
                            "version": env!("CARGO_PKG_VERSION"),
                        },
                        "authMethods": [],
                    }),
                );
            }
            "session/new" | "session/load" => {
                let loading = method == "session/load";
                let cwd = params
                    .get("cwd")
                    .and_then(Value::as_str)
                    .unwrap_or(".")
                    .to_string();
                let session_id = match params.get("sessionId").and_then(Value::as_str) {
                    Some(id) => validate_session_id(id),
                    None => Some(new_session_id("acp")),
                };
                let Some(session_id) = session_id else {
                    reply_error(&conn, id, "session: invalid sessionId");
                    continue;
                };
                // A turn holds its session's storage lock and marks it
                // busy; loading a busy id would open a SECOND handle on
                // the same JSONL mid-turn (CodeCora scan 2026-09-28).
                if lock_sessions(&sessions).is_busy(&session_id) {
                    reply_error(&conn, id, "session is busy running a turn");
                    continue;
                }
                // Shared approval handles: the bridge (allow_always grants),
                // the approver and the set_config_option lookup own the same
                // stores, seeded with the startup flags. The cancel token
                // (#178) is shared three ways: the prompt bridge (permission
                // wait), the session state (transport-side set) and the turn.
                let patterns = Arc::new(Mutex::new(allow_patterns.to_vec()));
                let session_auto_write = Arc::new(Mutex::new(auto_write));
                let cancel = tole_core::cancel::CancelToken::new();
                let approver = InteractiveApprover::new(AcpPrompt {
                    conn: conn.clone(),
                    session_id: session_id.clone(),
                    counter: Arc::new(Mutex::new(0)),
                    patterns: Arc::clone(&patterns),
                    cancel: cancel.clone(),
                })
                .with_allow_patterns(allow_patterns.to_vec())
                .with_auto_write(auto_write)
                .with_shared_patterns(Arc::clone(&patterns))
                .with_shared_auto_write(Arc::clone(&session_auto_write));
                // Plan publishing (issue #196 phase 4): ACP clients
                // render the standard `plan` session/update (Termul's
                // PlanPanel, full-replace semantics).
                let plan_emitter: tole_cli::tool_roster::PlanEmitter = {
                    let conn = conn.clone();
                    let sid = session_id.clone();
                    Arc::new(move |entries| {
                        conn.send_notification(
                            "session/update",
                            json!({
                                "sessionId": sid,
                                "update": {
                                    "sessionUpdate": "plan",
                                    "entries": entries,
                                }
                            }),
                        );
                    })
                };
                match open_session(
                    &session_id,
                    &cwd,
                    loading,
                    plan_mode,
                    approver,
                    memory.clone(),
                    sessions_dir.as_deref(),
                    turnend.clone(),
                    allow_patterns.to_vec(),
                    cancel,
                    Some(plan_emitter),
                ) {
                    Ok(state) => {
                        // Resolve the model list BEFORE taking the
                        // sessions lock: the lazy /models probe can block
                        // for PROBE_WAIT and must never run under it
                        // (issue #319).
                        let models = model_list();
                        // Insert + busy re-check in ONE critical section:
                        // open_session is slow (canonicalize + a git
                        // subprocess), and a prompt that set busy meanwhile
                        // must not be orphaned by the insert (CodeCora scan
                        // 2026-09-28).
                        // Model picker state (issue #176): the durable
                        // register override wins, else the env default,
                        // else the first advertised entry. Read BEFORE
                        // the state moves into the session map.
                        let model_override = session_model_override(&state.storage);
                        // The registry's admission rule: refuse an id that
                        // became busy while opening, replace an idle one in
                        // place, evict for a new id at capacity. Prune the
                        // approval state of every evicted session (#299).
                        match lock_sessions(&sessions).admit(session_id.clone(), state) {
                            Ok(evicted) => {
                                let mut approvals = approval_states.lock().expect("approval map");
                                for old in &evicted {
                                    approvals.remove(old);
                                }
                            }
                            Err(AdmitError::Busy) => {
                                reply_error(&conn, id, "session became busy while opening — retry");
                                continue;
                            }
                            Err(AdmitError::AtCapacity) => {
                                reply_error(
                                    &conn,
                                    id,
                                    "session map at capacity and all sessions busy — close one first",
                                );
                                continue;
                            }
                        }
                        let current = model_override
                            .or_else(|| env_model.clone())
                            .unwrap_or_default();
                        let session_auto =
                            *session_auto_write.lock().unwrap_or_else(|p| p.into_inner());
                        approval_states.lock().expect("approval map").insert(
                            session_id.clone(),
                            SessionApprovalState {
                                auto_write: session_auto_write,
                                current_model: Arc::new(Mutex::new(current.clone())),
                            },
                        );
                        let mut result = json!({ "sessionId": session_id });
                        if let Some(opts) = config_options_for(&current, &models, session_auto) {
                            result["configOptions"] = Value::Array(opts);
                        }
                        reply(&conn, id, result);
                    }
                    Err(e) => reply_error(&conn, id, &e),
                }
            }
            "session/prompt" => {
                let Some(session_id) = params
                    .get("sessionId")
                    .and_then(Value::as_str)
                    .map(str::to_string)
                else {
                    reply_error(&conn, id, "session/prompt: missing sessionId");
                    continue;
                };
                let (prompt_text, prompt_images) =
                    match prompt_content(params.get("prompt"), &|b64| {
                        tole_cli::turn_runner::resolve_inline_image(b64)
                    }) {
                        Ok(content) => content,
                        Err(e) => {
                            reply_error(&conn, id, &e);
                            continue;
                        }
                    };
                // The turn runs on its own thread; this reader loop stays
                // live for permission routing while it runs.
                let conn = conn.clone();
                let sessions = sessions.clone();
                let session_id_clone = session_id.clone();
                // The observer's id counter lives on the SESSION (cora
                // CI on #203): ids keep incrementing across turns so a
                // completion can never correlate onto a stale card.
                let tool_ids = lock_sessions(&sessions)
                    .get(&session_id)
                    .map(|st| st.tool_ids.clone())
                    .unwrap_or_default();
                std::thread::spawn(move || {
                    // Phase 3 dedup flag, shared with the closure below.
                    let text_streamed = Arc::new(std::sync::atomic::AtomicBool::new(false));
                    let observer: Arc<dyn tole_core::turn::TurnObserver> = Arc::new(AcpObserver {
                        conn: conn.clone(),
                        session_id: session_id_clone.clone(),
                        counter: tool_ids,
                        current: Mutex::new(None),
                        text_streamed: Arc::clone(&text_streamed),
                    });
                    match run_session_turn_detailed(
                        sessions,
                        &session_id_clone,
                        &prompt_text,
                        &prompt_images,
                        Some(Arc::clone(&observer)),
                        None,
                    ) {
                        Ok(r) => {
                            // Phase 3: deltas already delivered the text
                            // live; re-sending it would duplicate it.
                            let streamed = text_streamed.load(std::sync::atomic::Ordering::SeqCst);
                            if let Some(text) = r.text.filter(|_| !streamed) {
                                conn.send_notification(
                                    "session/update",
                                    json!({
                                        "sessionId": session_id_clone,
                                        "update": {
                                            "sessionUpdate": "agent_message_chunk",
                                            "content": {"type": "text", "text": text},
                                        }
                                    }),
                                );
                            }
                            reply(&conn, id, prompt_result(&r.stop, r.problem.as_ref()));
                        }
                        Err(e) => reply_error(&conn, id, &e),
                    }
                });
            }
            "session/set_config_option" => {
                set_config_option(
                    &conn,
                    id,
                    &sessions,
                    &approval_states,
                    &params,
                    &model_list(),
                    env_model.as_deref(),
                );
            }
            // ACP baseline MUST (issue #178): stop the session's generating
            // turn. A cancel BEFORE any prompt (or against an unknown
            // session) is a no-op. A cancel while a permission request is
            // pending also unwinds the wait (fail closed to Deny, per spec)
            // and the turn settles as `cancelled`, not `refusal`.
            "session/cancel" => {
                let Some(session_id) = params.get("sessionId").and_then(Value::as_str) else {
                    continue; // notification: no id, nothing to answer
                };
                let cancelled = lock_sessions(&sessions).cancel(session_id);
                if cancelled {
                    eprintln!("tole: session/{session_id} cancel requested");
                }
            }
            other => {
                if id.is_some() {
                    reply_error(&conn, id, &format!("method not supported: {other}"));
                }
            }
        }
    }
    Ok(())
}
