//! Opening and resuming the live sessions of `tole serve`: the shared open path of create
//! and resume, the live map and where the session files live.

use super::State;
use serde_json::json;
use std::sync::Arc;
use tole_cli::serve_support::profiles::{self, SessionProfile};
use tole_cli::session_host::{lock_sessions, open_session_with, SessionOverrides, Sessions};

/// Open (or reopen, `loading`) a serve session and admit it to the live map.
/// Shared by create and resume so both get the same approver, jail and
/// capacity rules. `Err` is the HTTP answer.
pub(super) fn open_serve_session(
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
pub(super) fn sessions_keeping_pending(
    approvals: &Arc<tole_cli::approvals::ApprovalQueue>,
) -> Sessions {
    let queue = Arc::clone(approvals);
    let mut sessions = Sessions::default();
    sessions.set_keep(Box::new(move |id| queue.has_pending(id)));
    sessions
}

/// Look up a session's context window off the request path (#573): the
/// provider's `/models` may be slow and the status route never waits for it.
pub(super) fn warm_status(state: &State, id: &str) {
    let st = lock_sessions(&state.sessions).get(id).cloned();
    if let Some(st) = st {
        std::thread::spawn(move || st.warm_status());
    }
}

/// Where this serve process keeps the session files of `cwd`.
pub(super) fn serve_storage_dir(state: &State, cwd: &str) -> std::path::PathBuf {
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
pub(super) fn resume_session(
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
