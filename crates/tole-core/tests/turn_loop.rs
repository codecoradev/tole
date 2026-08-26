//! E3 — Tier A tests: provider abstraction, tool registry, and the full
//! turn loop on the mock provider (no network, deterministic, fast).

use serde_json::{json, Value};
use tole_core::approval::{AllowlistApprover, Decision};
use tole_core::entry::Entry;
use tole_core::mock::MockProvider;
use tole_core::provider::{Provider, ProviderError, ProviderOutput};
use tole_core::storage::{JsonlStorage, Storage};
use tole_core::tool::{Risk, Tool, ToolRegistry};
use tole_core::turn::{run_turn, TurnOutcome, MAX_STEPS};

struct EchoTool;
impl Tool for EchoTool {
    fn name(&self) -> &str {
        "echo"
    }
    fn risk(&self) -> Risk {
        Risk::ReadOnly
    }
    fn execute(&self, input: Value) -> Result<Value, String> {
        Ok(input)
    }
}

struct FlakyTool;
impl Tool for FlakyTool {
    fn name(&self) -> &str {
        "flaky"
    }
    fn risk(&self) -> Risk {
        Risk::ReadOnly
    }
    fn execute(&self, _input: Value) -> Result<Value, String> {
        Err("tool exploded".into())
    }
}

struct WriteTool;
impl Tool for WriteTool {
    fn name(&self) -> &str {
        "write_file"
    }
    fn risk(&self) -> Risk {
        Risk::Write
    }
    fn execute(&self, input: Value) -> Result<Value, String> {
        Ok(input)
    }
}

fn tmpdir(name: &str) -> std::path::PathBuf {
    let d = std::env::temp_dir().join(format!("cora-e3-{}-{}", name, std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

// ---------------------------------------------------------------------------
// Provider trait + mock
// ---------------------------------------------------------------------------

#[test]
fn mock_replays_script_in_order_then_errors() {
    let mut p = MockProvider::scripted(vec![
        ProviderOutput::ToolCall {
            tool: "echo".into(),
            input: json!({"n": 1}),
        },
        ProviderOutput::Final {
            text: "done".into(),
        },
    ]);
    let t: Vec<Entry> = vec![];
    assert_eq!(
        p.complete(&t).unwrap(),
        ProviderOutput::ToolCall {
            tool: "echo".into(),
            input: json!({"n": 1})
        }
    );
    assert_eq!(
        p.complete(&t).unwrap(),
        ProviderOutput::Final {
            text: "done".into()
        }
    );
    assert!(matches!(p.complete(&t), Err(ProviderError(_))));
}

#[test]
fn mock_always_repeats() {
    let mut p = MockProvider::always(ProviderOutput::Final { text: "f".into() });
    let t: Vec<Entry> = vec![];
    for _ in 0..100 {
        assert!(p.complete(&t).is_ok());
    }
}

// ---------------------------------------------------------------------------
// Tool registry
// ---------------------------------------------------------------------------

#[test]
fn registry_refuses_duplicate_names() {
    let mut r = ToolRegistry::new();
    r.register(Box::new(EchoTool)).unwrap();
    let err = r.register(Box::new(EchoTool)).unwrap_err();
    assert!(err.contains("already registered"));
}

#[test]
fn registry_lookup_miss_returns_none() {
    let r = ToolRegistry::new();
    assert!(r.get("nope").is_none());
}

// ---------------------------------------------------------------------------
// Turn loop (full path, mock provider)
// ---------------------------------------------------------------------------

#[test]
fn turn_final_without_tools() {
    let dir = tmpdir("final");
    let mut s = JsonlStorage::create(&dir, "tf", None).unwrap();
    let mut p = MockProvider::scripted(vec![ProviderOutput::Final {
        text: "hello!".into(),
    }]);
    let mut reg = ToolRegistry::new();
    reg.register(Box::new(EchoTool)).unwrap();

    let out = run_turn(&mut s, &mut p, &reg, "hi").unwrap();
    match out {
        TurnOutcome::Final { text } => assert_eq!(text, "hello!"),
        other => panic!("expected Final, got {other:?}"),
    }
    // Durable state: user message + assistant message, pc Final.
    let msgs: Vec<&Entry> = s
        .entries()
        .iter()
        .filter(|e| e.kind.as_str() == "message")
        .collect();
    assert_eq!(msgs.len(), 2);
    assert_eq!(msgs[0].payload["role"], json!("user"));
    assert_eq!(msgs[1].payload["role"], json!("assistant"));
    assert_eq!(s.state().pc, tole_core::state::Pc::Final);
}

#[test]
fn turn_tool_call_then_final() {
    let dir = tmpdir("toolcall");
    let mut s = JsonlStorage::create(&dir, "tc", None).unwrap();
    let mut p = MockProvider::scripted(vec![
        ProviderOutput::ToolCall {
            tool: "echo".into(),
            input: json!({"n": 1}),
        },
        ProviderOutput::Final {
            text: "after tool".into(),
        },
    ]);
    let mut reg = ToolRegistry::new();
    reg.register(Box::new(EchoTool)).unwrap();

    let out = run_turn(&mut s, &mut p, &reg, "hi").unwrap();
    assert!(matches!(out, TurnOutcome::Final { .. }));

    // The sandwich left durable traces: intent + tool_result entries.
    let kinds: Vec<&str> = s.entries().iter().map(|e| e.kind.as_str()).collect();
    assert!(kinds.contains(&"intent"));
    assert!(kinds.contains(&"tool_result"));
    // Pending cell cleared after settlement.
    assert!(s.get_register("pending", "op").is_none());
    assert_eq!(s.state().pc, tole_core::state::Pc::Final);
}

#[test]
fn turn_unknown_tool_is_reported() {
    let dir = tmpdir("unknown");
    let mut s = JsonlStorage::create(&dir, "ut", None).unwrap();
    let mut p = MockProvider::scripted(vec![ProviderOutput::ToolCall {
        tool: "nonexistent".into(),
        input: json!({}),
    }]);
    let reg = ToolRegistry::new();

    let out = run_turn(&mut s, &mut p, &reg, "hi").unwrap();
    match out {
        TurnOutcome::UnknownTool { name } => assert_eq!(name, "nonexistent"),
        other => panic!("expected UnknownTool, got {other:?}"),
    }
}

#[test]
fn turn_write_tool_requires_approval_gate() {
    let dir = tmpdir("write");
    let mut s = JsonlStorage::create(&dir, "wt", None).unwrap();
    let mut p = MockProvider::scripted(vec![ProviderOutput::ToolCall {
        tool: "write_file".into(),
        input: json!({"path": "x"}),
    }]);
    let mut reg = ToolRegistry::new();
    // E4: no approver wired in — registration of a Write tool is refused
    // at the registry level (earlier and stricter than the old E3 behavior,
    // which registered everything and aborted per-call).
    let err = reg.register(Box::new(WriteTool)).unwrap_err();
    assert!(err.contains("approval gate"));

    let out = run_turn(&mut s, &mut p, &reg, "hi").unwrap();
    match out {
        TurnOutcome::UnknownTool { name } => assert_eq!(name, "write_file"),
        other => panic!("expected UnknownTool, got {other:?}"),
    }
    // The refusal is durable: an ERROR entry attached to the user message.
    assert!(!s.entries().iter().any(|e| e.kind.as_str() == "intent"));
    assert_eq!(s.state().pc, tole_core::state::Pc::Planning);
}

#[test]
fn turn_tool_failure_settles_error_and_replans() {
    let dir = tmpdir("fail");
    let mut s = JsonlStorage::create(&dir, "fl", None).unwrap();
    let mut p = MockProvider::scripted(vec![
        ProviderOutput::ToolCall {
            tool: "flaky".into(),
            input: json!({"n": 1}),
        },
        ProviderOutput::Final {
            text: "recovered".into(),
        },
    ]);
    let mut reg = ToolRegistry::new();
    reg.register(Box::new(FlakyTool)).unwrap();

    let out = run_turn(&mut s, &mut p, &reg, "hi").unwrap();
    assert!(matches!(out, TurnOutcome::Final { .. }));
    // Error settlement is durable: an error entry exists, pending cleared.
    assert!(s.entries().iter().any(|e| e.kind.as_str() == "error"));
    assert!(s.get_register("pending", "op").is_none());
    assert_eq!(s.state().pc, tole_core::state::Pc::Final);
}

#[test]
fn turn_budget_guard_trips_on_infinite_tool_loop() {
    let dir = tmpdir("budget");
    let mut s = JsonlStorage::create(&dir, "bg", None).unwrap();
    let mut p = MockProvider::always(ProviderOutput::ToolCall {
        tool: "echo".into(),
        input: json!({}),
    });
    let mut reg = ToolRegistry::new();
    reg.register(Box::new(EchoTool)).unwrap();

    let out = run_turn(&mut s, &mut p, &reg, "hi").unwrap();
    assert!(matches!(out, TurnOutcome::BudgetExhausted));
    // Exactly MAX_STEPS sandwiches were settled, all durable.
    let intents = s
        .entries()
        .iter()
        .filter(|e| e.kind.as_str() == "intent")
        .count();
    assert_eq!(intents, MAX_STEPS);
}

#[test]
fn turn_provider_failure_aborts_cleanly() {
    let dir = tmpdir("provfail");
    let mut s = JsonlStorage::create(&dir, "pf", None).unwrap();
    // Empty script: first complete() errors immediately.
    let mut p = MockProvider::scripted(vec![]);
    let reg = ToolRegistry::new();

    let out = run_turn(&mut s, &mut p, &reg, "hi").unwrap();
    match out {
        TurnOutcome::ProviderFailed { message } => {
            assert!(message.contains("script exhausted"))
        }
        other => panic!("expected ProviderFailed, got {other:?}"),
    }
    // Session still consistent: user message persisted, pc in Planning.
    assert_eq!(s.state().pc, tole_core::state::Pc::Planning);
    assert!(s
        .entries()
        .iter()
        .any(|e| e.kind.as_str() == "message" && e.payload["role"] == json!("user")));
}

#[test]
fn turn_survives_reopen_between_steps() {
    // Storage roundtrip mid-turn: run one step, drop storage, reopen,
    // continue with a second provider — the loop works off replayed state.
    let dir = tmpdir("reopen");
    let mut s = JsonlStorage::create(&dir, "rp", None).unwrap();
    let mut p = MockProvider::scripted(vec![ProviderOutput::ToolCall {
        tool: "echo".into(),
        input: json!({"n": 1}),
    }]);
    let mut reg = ToolRegistry::new();
    reg.register(Box::new(EchoTool)).unwrap();
    run_turn(&mut s, &mut p, &reg, "hi").unwrap();
    drop(s);

    let s2 = JsonlStorage::open(dir.join("rp.jsonl")).unwrap();
    // Full replay: both messages, intent, result — nothing lost.
    let kinds: Vec<&str> = s2.entries().iter().map(|e| e.kind.as_str()).collect();
    assert!(kinds.contains(&"message"));
    assert!(kinds.contains(&"intent"));
    assert!(kinds.contains(&"tool_result"));
}

// ---------------------------------------------------------------------------
// Review fixes (PR #17)
// ---------------------------------------------------------------------------

#[test]
fn turn_refused_when_pc_not_idle() {
    // First turn aborts mid-loop (provider failure) → pc is Planning.
    // A second run_turn on the same session must be refused, not silently
    // append another user message.
    let dir = tmpdir("notidle");
    let mut s = JsonlStorage::create(&dir, "ni", None).unwrap();
    let mut p = MockProvider::scripted(vec![]);
    let reg = ToolRegistry::new();
    let out = run_turn(&mut s, &mut p, &reg, "first").unwrap();
    assert!(matches!(out, TurnOutcome::ProviderFailed { .. }));
    assert_eq!(s.state().pc, tole_core::state::Pc::Planning);

    let err = run_turn(&mut s, &mut p, &reg, "second").unwrap_err();
    match err {
        tole_core::storage::StorageError::Invalid(msg) => {
            assert!(msg.contains("requires pc Idle"))
        }
        other => panic!("expected Invalid, got {other:?}"),
    }
    // Exactly one user message was persisted.
    let users = s
        .entries()
        .iter()
        .filter(|e| e.kind.as_str() == "message" && e.payload["role"] == json!("user"))
        .count();
    assert_eq!(users, 1);
}

// ---------------------------------------------------------------------------
// Durable abort records (PR #17 review)
// ---------------------------------------------------------------------------

#[test]
fn all_abort_paths_leave_durable_error_records() {
    for (name, tool, risk) in [
        ("unknown", "nonexistent", Risk::ReadOnly),
        ("approval", "write_file", Risk::Write),
    ] {
        let dir = tmpdir(name);
        let mut s = JsonlStorage::create(&dir, name, None).unwrap();
        let mut p = MockProvider::scripted(vec![ProviderOutput::ToolCall {
            tool: tool.into(),
            input: json!({}),
        }]);
        let mut reg = ToolRegistry::new();
        // Write tools need an approver (E4); only the approval path
        // registers one here via with_approver.
        if name == "approval" {
            reg = ToolRegistry::with_approver(AllowlistApprover::new(
                vec!["write_file".into()],
                Decision::Deny,
            ));
            reg.register(Box::new(WriteTool)).unwrap();
        }

        let _ = run_turn(&mut s, &mut p, &reg, "hi").unwrap();
        // Durable ERROR entry exists, attached to the user message.
        let errs: Vec<&tole_core::entry::Entry> = s
            .entries()
            .iter()
            .filter(|e| e.kind.as_str() == "error" && e.parent_id.is_some())
            .collect();
        assert_eq!(
            errs.len(),
            1,
            "abort path {name} must leave exactly one durable error"
        );
        let _ = risk;
    }
}
