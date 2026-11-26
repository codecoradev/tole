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

/// The serve prompt route's answer to a failed turn: 409 for a busy session
/// and for an interrupted turn (naming the register state), else 500.
pub fn prompt_error(message: String) -> (u16, Value) {
    match message.strip_prefix(PREFIX) {
        Some(state) => (409, json!({"error": "interrupted turn", "state": state})),
        None if message.contains("session is busy") => (409, json!({"error": message})),
        None => (500, json!({"error": message})),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_busy_and_interrupted_are_a_409() {
        let body = json!({"error": "interrupted turn", "state": "tool_call"});
        assert_eq!(prompt_error(format!("{PREFIX}tool_call")), (409, body));
        assert_eq!(prompt_error("session is busy: x".into()).0, 409);
        assert_eq!(prompt_error("boom".into()), (500, json!({"error": "boom"})));
    }
}
