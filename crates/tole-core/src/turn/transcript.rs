//! Read-side view of a session transcript: the conventions the turn loop
//! writes, answered next to the code that writes them.
//!
//! The turn loop marks three kinds of user-role entries that do NOT open a
//! turn (its own stop-gate feedback, a denial note #476, and an injected
//! interrupt, #377), and
//! stores tole's own data in usage rows under namespaced `tole_*` keys
//! (`tole_wire` #211, `tole_route` #209). Every reader — `tole status`,
//! the session list, usage reports, mission budgets, embedders — asks
//! here instead of re-encoding those rules.

use crate::entry::{Entry, EntryType};
use crate::storage::UsageRecord;
use serde_json::{json, Value};

/// Payload flag on the stop gate's own feedback message.
pub const STOP_GATE_FEEDBACK: &str = "stop_gate_feedback";
/// Payload flag on the synthetic note recording a denied tool call (#476).
pub const DENIAL_NOTE: &str = "denial_note";
/// Payload flag on an interrupt injected mid-turn.
pub const INTERRUPT: &str = "interrupt";
/// Usage-row key holding the request-size breakdown (#211).
pub const WIRE_KEY: &str = "tole_wire";
/// Usage-row key holding the provider route / failover record (#209).
pub const ROUTE_KEY: &str = "tole_route";
/// Usage-row key recording that a step ran with a rejected parameter
/// dropped (#457).
pub const DEGRADED_KEY: &str = "tole_degraded";
/// Prefix of every usage-row key tole writes itself.
const TOLE_KEY_PREFIX: &str = "tole_";

/// True when `e` opens a user turn: a user message that is neither the
/// stop gate's feedback nor an injected interrupt (the turn continues
/// after those).
pub fn opens_turn(e: &Entry) -> bool {
    e.kind.as_str() == EntryType::MESSAGE
        && e.payload["role"] == json!("user")
        && e.payload[STOP_GATE_FEEDBACK] != json!(true)
        && e.payload[DENIAL_NOTE] != json!(true)
        && e.payload[INTERRUPT] != json!(true)
}

/// True when the last entry is a denial note: the turn is parked after a
/// denied tool call (#476), so a new prompt may settle it.
pub fn parked_after_denial(entries: &[Entry]) -> bool {
    entries.last().is_some_and(|e| {
        e.kind.as_str() == EntryType::MESSAGE && e.payload[DENIAL_NOTE] == json!(true)
    })
}

/// Indices of the entries that open a turn, in order.
pub fn turn_starts(entries: &[Entry]) -> Vec<usize> {
    entries
        .iter()
        .enumerate()
        .filter(|(_, e)| opens_turn(e))
        .map(|(i, _)| i)
        .collect()
}

/// Number of user turns in the transcript.
pub fn turn_count(entries: &[Entry]) -> usize {
    entries.iter().filter(|e| opens_turn(e)).count()
}

/// Index of the entry that opened the current (last) turn, if any.
pub fn current_turn_start(entries: &[Entry]) -> Option<usize> {
    entries.iter().rposition(opens_turn)
}

/// True when the usage row carries a provider-reported usage object, i.e.
/// it is NOT a row holding only tole's own `tole_*` keys. The single
/// definition of "provider step" (mission step budget, `fact/mission`,
/// usage reports). An empty object still counts (the pre-#211 contract).
pub fn reports_provider_usage(rec: &UsageRecord) -> bool {
    match rec.usage.as_object() {
        Some(o) => o.is_empty() || o.keys().any(|k| !k.starts_with(TOLE_KEY_PREFIX)),
        None => true,
    }
}

/// Number of provider steps in a usage ledger.
pub fn provider_steps(usages: &[UsageRecord]) -> u64 {
    usages.iter().filter(|u| reports_provider_usage(u)).count() as u64
}

/// Prompt plus completion tokens over the ledger; rows without them count 0.
pub fn tokens_in_out(usages: &[UsageRecord]) -> u64 {
    let at = |u: &Value, k: &str| u.get(k).and_then(Value::as_u64).unwrap_or(0);
    usages
        .iter()
        .map(|u| at(&u.usage, "prompt_tokens").saturating_add(at(&u.usage, "completion_tokens")))
        .fold(0u64, u64::saturating_add)
}
