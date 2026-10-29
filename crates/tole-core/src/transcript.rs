//! Read-side view of a session transcript: the conventions the turn loop
//! writes, answered next to the code that writes them.
//!
//! The turn loop marks two kinds of user-role entries that do NOT open a
//! turn (its own stop-gate feedback and an injected interrupt, #377), and
//! stores tole's own data in usage rows under namespaced `tole_*` keys
//! (`tole_wire` #211, `tole_route` #209). Every reader — `tole status`,
//! the session list, usage reports, mission budgets, embedders — asks
//! here instead of re-encoding those rules.

use crate::entry::{Entry, EntryType};
use crate::storage::UsageRecord;
use serde_json::{json, Value};

/// Payload flag on the stop gate's own feedback message.
pub const STOP_GATE_FEEDBACK: &str = "stop_gate_feedback";
/// Payload flag on an interrupt injected mid-turn.
pub const INTERRUPT: &str = "interrupt";
/// Usage-row key holding the request-size breakdown (#211).
pub const WIRE_KEY: &str = "tole_wire";
/// Usage-row key holding the provider route / failover record (#209).
pub const ROUTE_KEY: &str = "tole_route";
/// Prefix of every usage-row key tole writes itself.
const TOLE_KEY_PREFIX: &str = "tole_";

/// True when `e` opens a user turn: a user message that is neither the
/// stop gate's feedback nor an injected interrupt (the turn continues
/// after those).
pub fn opens_turn(e: &Entry) -> bool {
    e.kind.as_str() == EntryType::MESSAGE
        && e.payload["role"] == json!("user")
        && e.payload[STOP_GATE_FEEDBACK] != json!(true)
        && e.payload[INTERRUPT] != json!(true)
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

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(kind: &'static str, payload: Value) -> Entry {
        Entry {
            seq: 0,
            id: String::new(),
            parent_id: None,
            kind: EntryType::new(kind),
            timestamp: 0,
            payload,
        }
    }

    fn rec(usage: Value) -> UsageRecord {
        UsageRecord {
            id: String::new(),
            entry_id: String::new(),
            usage,
            cost_usd: None,
        }
    }

    #[test]
    fn only_plain_user_messages_open_a_turn() {
        let entries = vec![
            entry("message", json!({"role": "user", "text": "a"})),
            entry("message", json!({"role": "assistant", "text": "b"})),
            entry(
                "message",
                json!({"role": "user", "text": "stop gate: x", STOP_GATE_FEEDBACK: true}),
            ),
            entry(
                "message",
                json!({"role": "user", "text": "[owner interrupt] y", INTERRUPT: true}),
            ),
            // A user-role payload on a non-message entry is not a turn.
            entry("entry", json!({"role": "user"})),
            entry("message", json!({"role": "user", "text": "c"})),
        ];
        assert_eq!(turn_starts(&entries), vec![0, 5]);
        assert_eq!(turn_count(&entries), 2);
        assert_eq!(current_turn_start(&entries), Some(5));
        assert_eq!(current_turn_start(&entries[1..5]), None);
    }

    #[test]
    fn rows_with_only_tole_keys_are_not_provider_steps() {
        let wire_only = rec(json!({ WIRE_KEY: {"system_chars": 1} }));
        let route_only = rec(json!({ ROUTE_KEY: {"provider": "a"} }));
        let real = rec(json!({"prompt_tokens": 3, ROUTE_KEY: {}}));
        assert!(!reports_provider_usage(&wire_only));
        assert!(!reports_provider_usage(&route_only));
        assert!(reports_provider_usage(&real));
        assert!(reports_provider_usage(&rec(json!({}))));
        assert!(reports_provider_usage(&rec(Value::Null)));
        assert_eq!(provider_steps(&[wire_only, route_only, real]), 1);
    }

    #[test]
    fn tokens_in_out_sums_and_tolerates_missing_fields() {
        let usages = [
            rec(json!({"prompt_tokens": 10, "completion_tokens": 2})),
            rec(json!({ WIRE_KEY: {} })),
            rec(json!({"prompt_tokens": u64::MAX})),
        ];
        assert_eq!(tokens_in_out(&usages[..2]), 12);
        assert_eq!(tokens_in_out(&usages), u64::MAX);
    }
}
