use serde_json::{json, Value};
use tole_core::entry::{Entry, EntryType};
use tole_core::storage::UsageRecord;
use tole_core::transcript::*;

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
        entry(
            "message",
            json!({"role": "user", "text": "[tole] denied", DENIAL_NOTE: true}),
        ),
        // A user-role payload on a non-message entry is not a turn.
        entry("entry", json!({"role": "user"})),
        entry("message", json!({"role": "user", "text": "c"})),
    ];
    assert_eq!(turn_starts(&entries), vec![0, 6]);
    assert_eq!(turn_count(&entries), 2);
    assert_eq!(current_turn_start(&entries), Some(6));
    assert_eq!(current_turn_start(&entries[1..6]), None);
    assert!(parked_after_denial(&entries[..5]));
    assert!(!parked_after_denial(&entries));
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
