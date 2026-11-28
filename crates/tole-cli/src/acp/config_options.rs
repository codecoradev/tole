//! The session config options (issue #176): the model picker and the approval selector.

use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use super::wire::{reply, reply_error, Conn};
use tole_cli::session_host::lock_sessions;
use tole_core::storage::Storage;

/// Durable per-session model override (issue #176): a `fact/model`
/// register write. Registers are the sanctioned append-only mutable
/// state, so the override survives `session/load` in a fresh process
/// and `run_session_turn` (shared with `tole serve`) applies it when
/// building the provider.
const MODEL_REGISTER: (&str, &str) = tole_cli::session_settings::MODEL;

/// Parse the `TOLE_MODELS` env into the advertised model list: comma
/// separated, trimmed, empties dropped, duplicates collapsed, order kept.
/// Empty result = no picker advertised (today's behavior).
pub(crate) fn parse_model_list(env_val: Option<&str>) -> Vec<String> {
    let Some(raw) = env_val else {
        return Vec::new();
    };
    let mut out: Vec<String> = Vec::new();
    for id in raw.split(',') {
        let id = id.trim();
        if !id.is_empty() && !out.iter().any(|m| m == id) {
            out.push(id.to_string());
        }
    }
    out
}

/// Model-list resolution precedence (issue #195): a non-empty `TOLE_MODELS`
/// wins AS-IS (deterministic, no network, the operator's override). Empty or
/// unset falls to `probe` (the provider's `GET /models`), used whatever it
/// returns, empty included (the caller caches it, see `ProbeCache`).
pub(super) fn resolve_model_list(
    env_val: Option<&str>,
    probe: impl FnOnce() -> Vec<String>,
) -> Vec<String> {
    let from_env = parse_model_list(env_val);
    if !from_env.is_empty() {
        return from_env;
    }
    probe()
}

/// How long a failed/timed-out `/models` probe is remembered (issue #319).
/// Within this window callers get an empty list instantly instead of paying
/// another `PROBE_WAIT` against a hanging gateway; after it, one retry is
/// allowed so a recovered gateway is picked up without restarting.
pub(super) const PROBE_FAIL_TTL: std::time::Duration = std::time::Duration::from_secs(30);

/// Cache for the lazy `/models` probe: a success is kept for the process
/// lifetime, a failure only until `PROBE_FAIL_TTL` elapses. The clock is a
/// parameter so the logic is testable without sleeping.
#[derive(Default)]
pub(super) struct ProbeCache {
    ok: Option<Vec<String>>,
    failed_at: Option<std::time::Instant>,
}

impl ProbeCache {
    /// `probe` returns `Some(non-empty list)` on success, `None` on any
    /// failure (error, empty list, timeout). It is only called on a cache
    /// miss (no success yet and no fresh failure).
    pub(super) fn get(
        &mut self,
        now: std::time::Instant,
        probe: impl FnOnce() -> Option<Vec<String>>,
    ) -> Vec<String> {
        if let Some(list) = &self.ok {
            return list.clone();
        }
        if let Some(at) = self.failed_at {
            if now.saturating_duration_since(at) < PROBE_FAIL_TTL {
                return Vec::new();
            }
        }
        match probe() {
            Some(list) => {
                self.ok = Some(list.clone());
                self.failed_at = None;
                list
            }
            None => {
                self.failed_at = Some(now);
                Vec::new()
            }
        }
    }
}

/// The full advertised option list: the `TOLE_MODELS` entries plus the
/// session's current model (prepended when missing — the spec requires
/// `currentValue` to be one of the options so the picker can render it).
fn advertised_models(current: &str, models: &[String]) -> Vec<String> {
    if current.is_empty() || models.iter().any(|m| m == current) {
        models.to_vec()
    } else {
        let mut all = vec![current.to_string()];
        all.extend(models.iter().cloned());
        all
    }
}

/// Build the `configOptions` entry for the model picker, or `None` when
/// there is nothing to advertise (no `TOLE_MODELS` configured). Wire shape
/// per the ACP spec: a `select` option with `category: "model"`. Select-only:
/// boolean options need the client's `session.configOptions.boolean`
/// capability, which tole does not probe.
pub(crate) fn model_config_option(current: &str, models: &[String]) -> Option<Value> {
    if models.is_empty() {
        return None;
    }
    let all = advertised_models(current, models);
    Some(json!({
        "id": "model",
        "name": "Model",
        "category": "model",
        "type": "select",
        "currentValue": current,
        "options": all
            .iter()
            .map(|m| json!({"value": m, "name": m}))
            .collect::<Vec<_>>(),
    }))
}

/// Build the `configOptions` entry for the approval selector: the per-session
/// `--yes` equivalent, category `mode`. Session-scoped by design: consent
/// resets with the process, unlike the model override.
fn approval_config_option(auto_write: bool) -> Value {
    json!({
        "id": "approval",
        "name": "Approval",
        "category": "mode",
        "type": "select",
        "currentValue": if auto_write { "auto" } else { "ask" },
        "options": [
            {"value": "ask", "name": "Ask",
             "description": "Request permission before Write tools"},
            {"value": "auto", "name": "Auto",
             "description": "Auto-allow Write tools (Destructive still prompts)"},
        ],
    })
}

/// The full `configOptions` response array for a session: the approval
/// selector always, the model picker when configured.
pub(super) fn config_options_for(
    current_model: &str,
    models: &[String],
    auto_write: bool,
) -> Option<Vec<Value>> {
    // The approval selector is unconditional; an early `?` on the model
    // fallback must not swallow it (found by the no-env E2E).
    let mut opts = vec![approval_config_option(auto_write)];
    if !models.is_empty() {
        let current = if current_model.is_empty() {
            models.first().expect("non-empty").clone()
        } else {
            current_model.to_string()
        };
        if let Some(model) = model_config_option(&current, models) {
            opts.push(model);
        }
    }
    Some(opts)
}

/// Read the durable model override from a session's storage.
pub(super) fn session_model_override(
    storage: &Arc<Mutex<tole_core::storage::JsonlStorage>>,
) -> Option<String> {
    let storage = storage.lock().unwrap_or_else(|p| p.into_inner());
    storage
        .get_register(MODEL_REGISTER.0, MODEL_REGISTER.1)
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

/// Per-session approval state, addressable by session id so
/// `session/set_config_option` can act without the storage mutex (the reader
/// thread must never block on a running turn). The auto-write handle is the
/// same `Arc` the session's `InteractiveApprover` owns; the effective model
/// mirrors `session/new` and is updated by `set_model_option`, so every
/// config-state reply shows the CURRENT selection (an approval flip must not
/// show a stale model). The pattern store stays bridge-owned (`AcpPrompt`).
#[derive(Clone)]
pub(super) struct SessionApprovalState {
    pub(super) auto_write: Arc<Mutex<bool>>,
    pub(super) current_model: Arc<Mutex<String>>,
}

/// `session/set_config_option` (issue #176): the client picked a new value
/// for `model` or `approval`.
///
/// Asymmetry is deliberate. `model` needs the session's storage mutex, so a
/// running turn refuses it (the reader loop would block on the lock the turn
/// holds: the permission-routing deadlock class the CodeCora scan removed),
/// and it only applies to the NEXT turn anyway. `approval` is an atomic flag
/// flip on shared handles, so it is honored even while generating: the very
/// next approval check sees it. Destructive tools are unaffected by either.
pub(super) fn set_config_option(
    conn: &Conn,
    id: Option<Value>,
    sessions: &tole_cli::session_host::SharedSessions,
    approval_states: &Arc<Mutex<HashMap<String, SessionApprovalState>>>,
    params: &Value,
    models: &[String],
    env_model: Option<&str>,
) {
    let Some(session_id) = params.get("sessionId").and_then(Value::as_str) else {
        reply_error(conn, id, "session/set_config_option: missing sessionId");
        return;
    };
    let Some(config_id) = params.get("configId").and_then(Value::as_str) else {
        reply_error(conn, id, "session/set_config_option: missing configId");
        return;
    };
    let Some(value) = params.get("value").and_then(Value::as_str) else {
        reply_error(conn, id, "session/set_config_option: missing value");
        return;
    };

    match config_id {
        "model" => set_model_option(
            conn,
            id,
            sessions,
            approval_states,
            session_id,
            value,
            models,
            env_model,
        ),
        "approval" => set_approval_option(conn, id, approval_states, session_id, value, models),
        other => reply_error(
            conn,
            id,
            &format!("session/set_config_option: unknown configId: {other}"),
        ),
    }
}

/// The `approval` config option: `ask` (per-call permission) or `auto`
/// (the `--yes` semantics — every Write auto-allowed, Destructive still
/// prompts). An atomic flip on the shared handle, visible to approval
/// checks immediately, including mid-turn. NOT persisted: consent
/// resets with the process by design.
fn set_approval_option(
    conn: &Conn,
    id: Option<Value>,
    approval_states: &Arc<Mutex<HashMap<String, SessionApprovalState>>>,
    session_id: &str,
    value: &str,
    models: &[String],
) {
    let auto = match value {
        "ask" => false,
        "auto" => true,
        _ => {
            reply_error(
                conn,
                id,
                &format!("session/set_config_option: unknown approval value: {value}"),
            );
            return;
        }
    };
    let state = {
        let map = approval_states.lock().expect("approval map");
        match map.get(session_id) {
            Some(st) => st.clone(),
            None => {
                reply_error(conn, id, &format!("unknown session: {session_id}"));
                return;
            }
        }
    };
    *state.auto_write.lock().unwrap_or_else(|p| p.into_inner()) = auto;
    let current = state
        .current_model
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .clone();
    let mut result = json!({});
    if let Some(opts) = config_options_for(&current, models, auto) {
        result["configOptions"] = Value::Array(opts);
    }
    reply(conn, id, result);
}

/// The `model` config option: persist the choice as a durable register
/// write and reply with the complete config state.
#[allow(clippy::too_many_arguments)]
fn set_model_option(
    conn: &Conn,
    id: Option<Value>,
    sessions: &tole_cli::session_host::SharedSessions,
    approval_states: &Arc<Mutex<HashMap<String, SessionApprovalState>>>,
    session_id: &str,
    value: &str,
    models: &[String],
    env_model: Option<&str>,
) {
    let env_current = env_model.unwrap_or_default();
    // Validate against exactly what this session advertises (including
    // the prepended env default): a value outside the list would break
    // the picker's currentValue ∈ options invariant.
    if !advertised_models(env_current, models)
        .iter()
        .any(|m| m == value)
    {
        reply_error(
            conn,
            id,
            &format!("session/set_config_option: unknown model value: {value}"),
        );
        return;
    }

    // Busy-check BEFORE touching the storage mutex: a running turn holds
    // the storage lock for its whole duration, and this handler runs on
    // the reader thread that routes permission requests.
    {
        let sessions = lock_sessions(sessions);
        let Some(state) = sessions.get(session_id) else {
            reply_error(conn, id, &format!("unknown session: {session_id}"));
            return;
        };
        if *state.busy.lock().expect("busy lock") {
            reply_error(conn, id, "session is busy running a turn");
            return;
        }
        // Durable write (append-only register entry), replayed by
        // session/load and honored by run_session_turn on every turn.
        let mut storage = state.storage.lock().unwrap_or_else(|p| p.into_inner());
        if let Err(e) = tole_cli::session_settings::write_model(&mut *storage, value) {
            reply_error(conn, id, &format!("storing model override: {e}"));
            return;
        }
    }
    // The spec reply carries the COMPLETE config state — approval entry
    // included, with its live values from the session cache.
    let state = {
        let map = approval_states.lock().expect("approval map");
        match map.get(session_id) {
            Some(st) => st.clone(),
            None => {
                reply_error(conn, id, &format!("unknown session: {session_id}"));
                return;
            }
        }
    };
    *state
        .current_model
        .lock()
        .unwrap_or_else(|p| p.into_inner()) = value.to_string();
    let auto = *state.auto_write.lock().unwrap_or_else(|p| p.into_inner());
    let mut result = json!({});
    if let Some(opts) = config_options_for(value, models, auto) {
        result["configOptions"] = Value::Array(opts);
    }
    reply(conn, id, result);
}
