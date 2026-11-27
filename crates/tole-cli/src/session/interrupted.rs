//! A prompt on a session whose turn was cut off mid-flight (#1230).
use serde_json::{json, Value};
use tole_core::{state::Pc, storage::Storage, turn::transcript::parked_after_denial};

const PREFIX: &str = "interrupted turn: ";

/// `Err(message)` when the register holds a turn that is not settled and not
/// parked after a denial: the condition of the core's `run_turn` refusal.
pub fn refuse(s: &dyn Storage) -> Result<(), String> {
    let pc = s.state().pc;
    let parked = pc == Pc::Planning && parked_after_denial(s.entries());
    if parked || matches!(pc, Pc::Idle | Pc::Final) {
        return Ok(());
    }
    let state = serde_json::to_value(pc).unwrap_or_default();
    Err(format!("{PREFIX}{}", state.as_str().unwrap_or_default()))
}

/// `POST /sessions {"resume"}` on an interrupted turn (#1230): settle it with
/// no model call and no tool run (`tole_core::turn::settle_interrupted`, append
/// only), under the busy claim, so a turn in flight is never touched. `Ok(true)`
/// when something was settled; `Ok(false)` for a clean session or a busy one.
pub fn settle(sessions: &super::host::SharedSessions, id: &str) -> Result<bool, String> {
    let claimed = super::host::lock_sessions(sessions).claim_turn(id);
    let (state, _busy) = match claimed {
        Ok(c) => c,
        Err(e) if e.contains("busy") => return Ok(false),
        Err(e) => return Err(e),
    };
    let mut storage = state.storage.lock().unwrap_or_else(|p| p.into_inner());
    tole_core::turn::settle_interrupted(&mut *storage).map_err(|e| e.to_string())
}

/// The serve prompt route's answer to a failed turn: 409 for a busy session
/// and for an interrupted turn (naming the register state), else 500.
pub fn prompt_error(message: String) -> (u16, Value) {
    match message.strip_prefix(PREFIX) {
        Some(state) => (409, json!({"error": "interrupted turn", "state": state})),
        None if message.contains("session is busy") => (409, json!({"error": message})),
        None => (500, json!({"error": message})),
    }
}
