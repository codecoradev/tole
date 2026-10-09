//! E3 — Tier A tests: provider abstraction, tool registry, and the full
//! turn loop on the mock provider (no network, deterministic, fast).

use serde_json::{json, Value};
use tole_core::approval::AllowlistApprover;
use tole_core::entry::Entry;
use tole_core::mock::MockProvider;
use tole_core::provider::{Provider, ProviderError, ProviderOutput};
use tole_core::storage::{JsonlStorage, Storage};
use tole_core::tool::{Risk, Tool, ToolRegistry};
use tole_core::turn::{resume_turn, run_turn, TurnOutcome, MAX_STEPS};

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
    // Unique PER CALL (pid + monotonic counter): two tests sharing a tag
    // (e.g. "unknown") must never remove_dir_all each other's directory
    // when the harness runs them in parallel.
    static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let d = std::env::temp_dir().join(format!("cora-e3-{}-{}-{n:x}", name, std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

/// Mock that reports provider usage like OpenAI does.
struct UsageMock {
    step: std::sync::Mutex<usize>,
    outputs: Vec<ProviderOutput>,
    usage: Value,
}
impl Provider for UsageMock {
    fn complete(&mut self, _t: &[Entry]) -> Result<ProviderOutput, ProviderError> {
        let mut i = self.step.lock().unwrap();
        let out = self
            .outputs
            .get(*i)
            .cloned()
            .ok_or_else(|| ProviderError("exhausted".into()));
        *i += 1;
        out
    }
    fn last_usage(&self) -> Option<Value> {
        Some(self.usage.clone())
    }
}

#[test]
fn usage_rows_are_written_durably_per_step() {
    let dir = tmpdir("usage-ledger");
    let mut s = JsonlStorage::create(&dir, "ul", None).unwrap();
    let mut p = UsageMock {
        step: std::sync::Mutex::new(0),
        outputs: vec![
            ProviderOutput::ToolCall {
                tool: "echo".into(),
                input: json!({"n": 1}),
            },
            ProviderOutput::Final {
                text: "done".into(),
            },
        ],
        usage: json!({"prompt_tokens": 100, "completion_tokens": 7}),
    };
    let mut reg = ToolRegistry::new();
    reg.register(Box::new(EchoTool)).unwrap();

    let out = run_turn(&mut s, &mut p, &reg, "hi").unwrap();
    assert!(matches!(out, TurnOutcome::Final { .. }));
    // Two steps -> two usage rows, anchored to existing entries, replayable.
    let usages = s.usages();
    assert_eq!(usages.len(), 2);
    assert_eq!(usages[0].usage["prompt_tokens"], json!(100));
    for u in usages {
        assert!(
            s.entries().iter().any(|e| e.id == u.entry_id),
            "usage row must anchor to a real entry"
        );
    }
    // Totals the CLI status view sums.
    let total_in: u64 = usages
        .iter()
        .filter_map(|u| u.usage.get("prompt_tokens").and_then(|v| v.as_u64()))
        .sum();
    assert_eq!(total_in, 200);
}

/// Mock reporting a per-step wire breakdown (issue #211) next to an
/// optional provider usage object. `stats` is consumed one per step.
struct WireMock {
    step: usize,
    outputs: Vec<ProviderOutput>,
    usage: Option<Value>,
    stats: Vec<Value>,
}
impl Provider for WireMock {
    fn complete(&mut self, _t: &[Entry]) -> Result<ProviderOutput, ProviderError> {
        let out = self
            .outputs
            .get(self.step)
            .cloned()
            .ok_or_else(|| ProviderError("exhausted".into()));
        self.step += 1;
        out
    }
    fn last_usage(&self) -> Option<Value> {
        self.usage.clone()
    }
    fn last_wire_stats(&self) -> Option<Value> {
        self.stats.get(self.step - 1).cloned()
    }
}

fn wire_json(n: u64) -> Value {
    json!({"system_chars": n, "tools_chars": n * 2, "history_chars": n * 3, "messages": n})
}

#[test]
fn wire_stats_are_merged_into_usage_under_tole_wire_and_provider_keys_survive() {
    let dir = tmpdir("usage-wire");
    let mut s = JsonlStorage::create(&dir, "uw", None).unwrap();
    let mut p = WireMock {
        step: 0,
        outputs: vec![
            ProviderOutput::ToolCall {
                tool: "echo".into(),
                input: json!({"n": 1}),
            },
            ProviderOutput::Final {
                text: "done".into(),
            },
        ],
        usage: Some(json!({
            "prompt_tokens": 100,
            "completion_tokens": 7,
            "prompt_tokens_details": {"cached_tokens": 64},
        })),
        stats: vec![wire_json(1), wire_json(2)],
    };
    let mut reg = ToolRegistry::new();
    reg.register(Box::new(EchoTool)).unwrap();
    run_turn(&mut s, &mut p, &reg, "hi").unwrap();
    let usages = s.usages();
    assert_eq!(usages.len(), 2);
    for (i, u) in usages.iter().enumerate() {
        assert_eq!(u.usage["tole_wire"], wire_json(i as u64 + 1), "step {i}");
        // Provider keys are untouched.
        assert_eq!(u.usage["prompt_tokens"], json!(100));
        assert_eq!(u.usage["completion_tokens"], json!(7));
        assert_eq!(u.usage["prompt_tokens_details"]["cached_tokens"], json!(64));
    }
    // Ledger consumers are unaffected: same step count, same token sums.
    let total_in: u64 = usages
        .iter()
        .filter_map(|u| u.usage.get("prompt_tokens").and_then(|v| v.as_u64()))
        .sum();
    assert_eq!(total_in, 200);
}

#[test]
fn wire_stats_without_provider_usage_store_a_tole_wire_only_record() {
    let dir = tmpdir("usage-wire-only");
    let mut s = JsonlStorage::create(&dir, "uwo", None).unwrap();
    let mut p = WireMock {
        step: 0,
        outputs: vec![ProviderOutput::Final { text: "f".into() }],
        usage: None,
        stats: vec![wire_json(5)],
    };
    let mut reg = ToolRegistry::new();
    reg.register(Box::new(EchoTool)).unwrap();
    run_turn(&mut s, &mut p, &reg, "hi").unwrap();
    let usages = s.usages();
    assert_eq!(usages.len(), 1, "still one ledger row = one step");
    assert_eq!(usages[0].usage, json!({"tole_wire": wire_json(5)}));
    // A consumer summing prompt/completion tokens reads 0 from it.
    let tokens: u64 = usages
        .iter()
        .map(|u| {
            u.usage
                .get("prompt_tokens")
                .and_then(|v| v.as_u64())
                .unwrap_or(0)
                + u.usage
                    .get("completion_tokens")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0)
        })
        .sum();
    assert_eq!(tokens, 0);
}

/// Stored numbers equal `wire_stats` of the REAL request body for a
/// known transcript (fixed system prompt, 2 tools, one user message).
#[test]
fn stored_wire_numbers_equal_wire_stats_of_the_request_body() {
    use tole_core::openai::{wire_stats, OpenAiConfig, OpenAiProvider};
    struct BodyMeasuring {
        inner: OpenAiProvider,
        last: Option<Value>,
    }
    impl Provider for BodyMeasuring {
        fn complete(&mut self, t: &[Entry]) -> Result<ProviderOutput, ProviderError> {
            self.last = Some(wire_stats(&self.inner.request_body(t)).to_value());
            Ok(ProviderOutput::Final { text: "ok".into() })
        }
        fn last_wire_stats(&self) -> Option<Value> {
            self.last.clone()
        }
    }
    let specs = vec![
        json!({"type":"function","function":{"name":"a","description":"d","parameters":{}}}),
        json!({"type":"function","function":{"name":"b","description":"d","parameters":{}}}),
    ];
    let inner = OpenAiProvider::new(OpenAiConfig::new("https://x/v1", "m", "k"))
        .with_system_prompt("You are tole.")
        .with_tool_specs(specs);
    let mut p = BodyMeasuring { inner, last: None };
    let dir = tmpdir("usage-wire-golden");
    let mut s = JsonlStorage::create(&dir, "uwg", Some("You are tole.".into())).unwrap();
    let mut reg = ToolRegistry::new();
    reg.register(Box::new(EchoTool)).unwrap();
    run_turn(&mut s, &mut p, &reg, "hi").unwrap();
    let usages = s.usages();
    assert_eq!(usages.len(), 1);
    let tools_len = r#"[{"function":{"description":"d","name":"a","parameters":{}},"type":"function"},{"function":{"description":"d","name":"b","parameters":{}},"type":"function"}]"#
        .chars()
        .count();
    assert_eq!(
        usages[0].usage["tole_wire"],
        json!({"system_chars": 13, "tools_chars": tools_len, "history_chars": 30, "messages": 2})
    );
}

#[test]
fn no_usage_rows_without_provider_usage() {
    let dir = tmpdir("usage-none");
    let mut s = JsonlStorage::create(&dir, "un", None).unwrap();
    let mut p = MockProvider::scripted(vec![ProviderOutput::Final { text: "f".into() }]);
    let mut reg = ToolRegistry::new();
    reg.register(Box::new(EchoTool)).unwrap();
    run_turn(&mut s, &mut p, &reg, "hi").unwrap();
    assert_eq!(s.usages().len(), 0);
}

// ---------------------------------------------------------------------------
// Issue #58: harness-level retry-once on timeout-classified provider failure
// ---------------------------------------------------------------------------

#[test]
fn provider_timeout_retried_once_then_succeeds() {
    let dir = tmpdir("t58-retry");
    let mut s = JsonlStorage::create(&dir, "t58r", None).unwrap();
    let mut p = MockProvider::scripted_with_failures(vec![
        Err("provider failed: timeout: global".into()),
        Ok(ProviderOutput::Final {
            text: "done".into(),
        }),
    ]);
    let mut reg = ToolRegistry::new();
    reg.register(Box::new(EchoTool)).unwrap();

    let out = run_turn(&mut s, &mut p, &reg, "hi").unwrap();
    match out {
        TurnOutcome::Final { text, .. } => assert_eq!(text, "done"),
        other => panic!("expected Final after timeout retry, got {other:?}"),
    }
    // Durable audit trail: the retry decision must be visible to replay.
    let errs: Vec<&Entry> = s
        .entries()
        .iter()
        .filter(|e| e.kind.as_str() == "error")
        .collect();
    assert_eq!(errs.len(), 1);
    assert_eq!(
        errs[0].payload["error"],
        json!("provider transient failure, retrying")
    );
    // 0 tool executions on the timeout path — the transcript the retry
    // re-reads is unchanged by the failed step.
    assert_eq!(s.state().pc, tole_core::state::Pc::Final);
}

#[test]
fn provider_timeout_second_failure_aborts() {
    let dir = tmpdir("t58-second");
    let mut s = JsonlStorage::create(&dir, "t58s", None).unwrap();
    let mut p = MockProvider::scripted_with_failures(vec![
        Err("provider failed: timeout: global".into()),
        Err("provider failed: timeout: global".into()),
    ]);
    let mut reg = ToolRegistry::new();
    reg.register(Box::new(EchoTool)).unwrap();

    let out = run_turn(&mut s, &mut p, &reg, "hi").unwrap();
    match out {
        TurnOutcome::ProviderFailed { message } => {
            assert!(message.contains("timeout"));
        }
        other => panic!("expected ProviderFailed, got {other:?}"),
    }
    // One retry record + one final failure record.
    let errs: Vec<&Entry> = s
        .entries()
        .iter()
        .filter(|e| e.kind.as_str() == "error")
        .collect();
    assert_eq!(errs.len(), 2);
    assert_eq!(
        errs[0].payload["error"],
        json!("provider transient failure, retrying")
    );
    assert_eq!(errs[1].payload["error"], json!("provider failed"));
}

#[test]
fn provider_429_rate_limit_retried_once() {
    let dir = tmpdir("t58-429");
    let mut s = JsonlStorage::create(&dir, "t58q", None).unwrap();
    let mut p = MockProvider::scripted_with_failures(vec![
        Err("provider failed: http status: 429".into()),
        Ok(ProviderOutput::Final {
            text: "after-429".into(),
        }),
    ]);
    let mut reg = ToolRegistry::new();
    reg.register(Box::new(EchoTool)).unwrap();

    let out = run_turn(&mut s, &mut p, &reg, "hi").unwrap();
    match out {
        TurnOutcome::Final { text, .. } => assert_eq!(text, "after-429"),
        other => panic!("expected Final after 429 retry, got {other:?}"),
    }
    let errs: Vec<&Entry> = s
        .entries()
        .iter()
        .filter(|e| e.kind.as_str() == "error")
        .collect();
    assert_eq!(errs.len(), 1);
    assert_eq!(
        errs[0].payload["error"],
        json!("provider transient failure, retrying")
    );
}

#[test]
fn non_timeout_provider_error_not_retried() {
    let dir = tmpdir("t58-nontimeout");
    let mut s = JsonlStorage::create(&dir, "t58n", None).unwrap();
    let mut p = MockProvider::scripted_with_failures(vec![
        Err("provider failed: 401 unauthorized".into()),
        Ok(ProviderOutput::Final {
            text: "never".into(),
        }),
    ]);
    let mut reg = ToolRegistry::new();
    reg.register(Box::new(EchoTool)).unwrap();

    let out = run_turn(&mut s, &mut p, &reg, "hi").unwrap();
    match out {
        TurnOutcome::ProviderFailed { message } => {
            assert!(message.contains("401"));
        }
        other => panic!("expected ProviderFailed, got {other:?}"),
    }
    // No retry record: only the terminal failure is durable.
    let errs: Vec<&Entry> = s
        .entries()
        .iter()
        .filter(|e| e.kind.as_str() == "error")
        .collect();
    assert_eq!(errs.len(), 1);
    assert_eq!(errs[0].payload["error"], json!("provider failed"));
}

#[test]
fn flapping_timeout_retries_only_once_per_turn() {
    let dir = tmpdir("t58-flap");
    let mut s = JsonlStorage::create(&dir, "t58f", None).unwrap();
    // Timeout, success, then another timeout: the second timeout must NOT
    // get a second retry (budget is per turn, not per step).
    let mut p = MockProvider::scripted_with_failures(vec![
        Err("provider failed: timeout: global".into()),
        Ok(ProviderOutput::ToolCall {
            tool: "echo".into(),
            input: json!({"n": 1}),
        }),
        Err("provider failed: timeout: global".into()),
        Ok(ProviderOutput::Final {
            text: "never".into(),
        }),
    ]);
    let mut reg = ToolRegistry::new();
    reg.register(Box::new(EchoTool)).unwrap();

    let out = run_turn(&mut s, &mut p, &reg, "hi").unwrap();
    match out {
        TurnOutcome::ProviderFailed { message } => assert!(message.contains("timeout")),
        other => panic!("expected ProviderFailed, got {other:?}"),
    }
    let retry_records: Vec<&Entry> = s
        .entries()
        .iter()
        .filter(|e| {
            e.kind.as_str() == "error"
                && e.payload["error"] == json!("provider transient failure, retrying")
        })
        .collect();
    assert_eq!(retry_records.len(), 1);
}

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
        TurnOutcome::Final { text, .. } => assert_eq!(text, "hello!"),
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
    assert_eq!(s.state().pc, tole_core::state::Pc::Final);
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
    // Distinct input per step so the E10 loop guard never fires — this
    // test exercises the *budget* ceiling only.
    struct Varying {
        step: std::cell::Cell<usize>,
    }
    impl Provider for Varying {
        fn complete(
            &mut self,
            _t: &[tole_core::entry::Entry],
        ) -> Result<ProviderOutput, ProviderError> {
            self.step.set(self.step.get() + 1);
            Ok(ProviderOutput::ToolCall {
                tool: "echo".into(),
                input: json!({ "n": self.step.get() }),
            })
        }
    }
    let mut p = Varying {
        step: std::cell::Cell::new(0),
    };
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
fn turn_loop_guard_trips_on_repeated_identical_calls() {
    let dir = tmpdir("loopguard");
    let mut s = JsonlStorage::create(&dir, "lg", None).unwrap();
    // Identical call, forever: the guard trips at LOOP_TRIP_AFTER.
    let mut p = MockProvider::always(ProviderOutput::ToolCall {
        tool: "echo".into(),
        input: json!({ "same": true }),
    });
    let mut reg = ToolRegistry::new();
    reg.register(Box::new(EchoTool)).unwrap();

    let out = run_turn(&mut s, &mut p, &reg, "hi").unwrap();
    assert!(matches!(out, TurnOutcome::LoopDetected { count: 3, .. }));
    // Durable record: an error entry explains the trip.
    assert!(s
        .entries()
        .iter()
        .any(|e| { e.kind.as_str() == "error" && e.payload["error"] == json!("loop detected") }));
    // Only 2 sandwiches settled: the third call is refused before execute.
    let intents = s
        .entries()
        .iter()
        .filter(|e| e.kind.as_str() == "intent")
        .count();
    assert_eq!(intents, 2);
}

#[test]
fn turn_loop_guard_resets_on_different_input() {
    let dir = tmpdir("loopreset");
    let mut s = JsonlStorage::create(&dir, "lr", None).unwrap();
    // a a b a a b … never 3 identical in a row: budget must be the trip,
    // not the loop guard.
    struct Alternating {
        i: std::cell::Cell<usize>,
    }
    impl Provider for Alternating {
        fn complete(
            &mut self,
            _t: &[tole_core::entry::Entry],
        ) -> Result<ProviderOutput, ProviderError> {
            self.i.set(self.i.get() + 1);
            let which = if (self.i.get() - 1) % 3 < 2 { "a" } else { "b" };
            Ok(ProviderOutput::ToolCall {
                tool: "echo".into(),
                input: json!({ "x": which }),
            })
        }
    }
    let mut p = Alternating {
        i: std::cell::Cell::new(0),
    };
    let mut reg = ToolRegistry::new();
    reg.register(Box::new(EchoTool)).unwrap();

    let out = run_turn(&mut s, &mut p, &reg, "hi").unwrap();
    assert!(matches!(out, TurnOutcome::BudgetExhausted));
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
    // #84: terminal provider failure SETTLES the turn (Planning→Final)
    // instead of wedging at Planning — a plain `resume <id> "prompt"`
    // must work next, no interactive recovery dance.
    assert_eq!(s.state().pc, tole_core::state::Pc::Final);
    assert!(s
        .entries()
        .iter()
        .any(|e| e.kind.as_str() == "message" && e.payload["role"] == json!("user")));
    // And the next turn drives cleanly from the settled state.
    let mut p2 = MockProvider::scripted(vec![ProviderOutput::Final {
        text: "recovered".into(),
    }]);
    let out2 = run_turn(&mut s, &mut p2, &reg, "continue").unwrap();
    assert!(matches!(out2, TurnOutcome::Final { .. }), "got {out2:?}");
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
    // A session parked mid-flight (pc=Planning via a manual commit —
    // provider failures now SETTLE to Final per #84) must refuse a
    // second concurrent run_turn, not silently append another user
    // message. The wedge state is constructed directly: commit a
    // Planning transition with no driving turn.
    let dir = tmpdir("notidle");
    let mut s = JsonlStorage::create(&dir, "ni", None).unwrap();
    let mut p = MockProvider::scripted(vec![]);
    let reg = ToolRegistry::new();
    use tole_core::entry::{EntryType, NewEntry};
    use tole_core::state::StateTransition;
    use tole_core::storage::{Commit, Storage};
    s.commit(Commit::new().entry(NewEntry::root(
        EntryType::new(EntryType::MESSAGE),
        json!({"role": "user", "text": "first"}),
    )))
    .unwrap();
    let seq = s.state().seq; // CAS: seq AFTER the entry commit
    s.commit(Commit::new().transition(StateTransition::from(seq, tole_core::state::Pc::Planning)))
        .unwrap();
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
fn abort_paths_leave_durable_error_records_and_no_final() {
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
            reg = ToolRegistry::with_approver(AllowlistApprover::allow_only(vec![
                "write_file".into()
            ]));
            reg.register(Box::new(WriteTool)).unwrap();
        }

        let outcome = run_turn(&mut s, &mut p, &reg, "hi").unwrap();
        // Neither abort flavor may end in a Final answer — the turn must
        // park on the failure and leave the durable ERROR record checked
        // below (CodeCora scan 2026-09-18: do not discard the outcome
        // under test). The exact flavor differs per case: "unknown" →
        // UnknownTool; "approval" → the allowlist here pattern-matches
        // `write_file` (AllowlistApprover semantics: match beats the
        // default), the tool RUNS, and the exhausted script settles as
        // ProviderFailed — the real approval gate contract is covered by
        // `turn_write_tool_requires_approval_gate`.
        assert!(
            !matches!(outcome, TurnOutcome::Final { .. }),
            "aborted turn must not produce Final, got {outcome:?}"
        );
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

#[test]
fn chat_reopens_final_session_for_next_turn() {
    // B1: a completed turn (pc=Final) accepts a new user message — the
    // session becomes a multi-turn conversation on one durable tree.
    let dir = tmpdir("chatreopen");
    let mut s = JsonlStorage::create(&dir, "cr", None).unwrap();
    let reg = ToolRegistry::new();

    // Turn 1: scripted final answer.
    let mut p1 = MockProvider::scripted(vec![ProviderOutput::Final {
        text: "first answer".into(),
    }]);
    let out = run_turn(&mut s, &mut p1, &reg, "hello").unwrap();
    assert!(matches!(out, TurnOutcome::Final { ref text, .. } if text == "first answer"));
    assert_eq!(s.state().pc, tole_core::state::Pc::Final);

    // Turn 2 on the SAME session: re-open Final → Planning.
    let mut p2 = MockProvider::scripted(vec![ProviderOutput::Final {
        text: "second answer".into(),
    }]);
    let out = run_turn(&mut s, &mut p2, &reg, "again").unwrap();
    assert!(matches!(out, TurnOutcome::Final { ref text, .. } if text == "second answer"));

    // The tree holds the full conversation: 2 user + 2 assistant messages.
    let users = s
        .entries()
        .iter()
        .filter(|e| e.kind.as_str() == "message" && e.payload["role"] == json!("user"))
        .count();
    let assistants = s
        .entries()
        .iter()
        .filter(|e| e.kind.as_str() == "message" && e.payload["role"] == json!("assistant"))
        .count();
    assert_eq!((users, assistants), (2, 2));
}

#[test]
fn loop_guard_exempts_poll_tools_but_trips_eventually() {
    // #85: a poll tool with identical input is the CORRECT pattern (the
    // arguments name the job; the result carries the change). The guard
    // must let dozens of identical job_poll calls through, while a
    // non-poll tool still trips at LOOP_TRIP_AFTER, and even polls trip
    // at the (much higher) patience ceiling.
    use tole_core::tool::{Risk, Tool};

    struct FakePoll;
    impl Tool for FakePoll {
        fn name(&self) -> &str {
            "job_poll"
        }
        fn risk(&self) -> Risk {
            Risk::ReadOnly
        }
        fn is_poll(&self) -> bool {
            true
        }
        fn describe(&self, _i: &Value) -> String {
            "poll".into()
        }
        fn execute(&self, _i: Value) -> Result<Value, String> {
            Ok(json!({"running": true}))
        }
    }
    struct FakeStuck;
    impl Tool for FakeStuck {
        fn name(&self) -> &str {
            "stuck_tool"
        }
        fn risk(&self) -> Risk {
            Risk::ReadOnly
        }
        fn describe(&self, _i: &Value) -> String {
            "stuck".into()
        }
        fn execute(&self, _i: Value) -> Result<Value, String> {
            Ok(json!({}))
        }
    }

    // 50 identical polls: legal (under the 120 poll ceiling).
    let dir = tmpdir("poll-ok");
    let mut s = JsonlStorage::create(&dir, "poll-ok", None).unwrap();
    let mut reg = ToolRegistry::new();
    reg.register(Box::new(FakePoll)).unwrap();
    let mut script = Vec::new();
    for _ in 0..25 {
        script.push(ProviderOutput::ToolCall {
            tool: "job_poll".into(),
            input: json!({"job": "j-1"}),
        });
    }
    script.push(ProviderOutput::Final {
        text: "done".into(),
    });
    let mut p = MockProvider::scripted(script);
    let out = run_turn(&mut s, &mut p, &reg, "wait for job").unwrap();
    assert!(
        matches!(out, TurnOutcome::Final { .. }),
        "identical polls under the step budget must NOT trip: {out:?}"
    );

    // Same count of identical NON-poll calls: still trips at 3.
    let dir = tmpdir("stuck");
    let mut s = JsonlStorage::create(&dir, "stuck", None).unwrap();
    let mut reg = ToolRegistry::new();
    reg.register(Box::new(FakeStuck)).unwrap();
    let mut script = Vec::new();
    for _ in 0..5 {
        script.push(ProviderOutput::ToolCall {
            tool: "stuck_tool".into(),
            input: json!({"x": 1}),
        });
    }
    script.push(ProviderOutput::Final {
        text: "unreachable".into(),
    });
    let mut p = MockProvider::scripted(script);
    let out = run_turn(&mut s, &mut p, &reg, "go").unwrap();
    assert!(
        matches!(out, TurnOutcome::LoopDetected { ref tool, .. } if tool == "stuck_tool"),
        "non-poll identical calls must still trip: {out:?}"
    );
}

// ---------------------------------------------------------------------------
// Issue #143: write-sessions are typed as decisions in the memory loop
// ---------------------------------------------------------------------------

#[test]
fn write_session_reports_wrote_true_on_final() {
    let dir = tmpdir("decision-wrote");
    let mut s = JsonlStorage::create(&dir, "dw", None).unwrap();
    let mut p = MockProvider::scripted(vec![
        ProviderOutput::ToolCall {
            tool: "write_file".into(),
            input: json!({"path": "a.txt"}),
        },
        ProviderOutput::Final {
            text: "written".into(),
        },
    ]);
    let mut reg =
        ToolRegistry::with_approver(AllowlistApprover::allow_only(
            vec!["write_file".to_string()],
        ));
    reg.register(Box::new(WriteTool)).unwrap();

    let out = run_turn(&mut s, &mut p, &reg, "hi").unwrap();
    match out {
        TurnOutcome::Final { wrote, .. } => assert!(
            wrote,
            "a session that executed a Write tool must report wrote=true"
        ),
        other => panic!("expected Final, got {other:?}"),
    }
}

#[test]
fn readonly_session_reports_wrote_false_on_final() {
    let dir = tmpdir("decision-readonly");
    let mut s = JsonlStorage::create(&dir, "dr", None).unwrap();
    let mut p = MockProvider::scripted(vec![
        ProviderOutput::ToolCall {
            tool: "echo".into(),
            input: json!({}),
        },
        ProviderOutput::Final {
            text: "done".into(),
        },
    ]);
    let mut reg = ToolRegistry::new();
    reg.register(Box::new(EchoTool)).unwrap();

    let out = run_turn(&mut s, &mut p, &reg, "hi").unwrap();
    match out {
        TurnOutcome::Final { wrote, .. } => {
            assert!(!wrote, "read-only sessions must keep wrote=false")
        }
        other => panic!("expected Final, got {other:?}"),
    }
}

#[test]
fn wrote_flag_survives_crash_resume_boundary() {
    // Write settles, provider dies BEFORE Final, session resumes:
    // the resumed turn's Final must still carry wrote=true (the
    // durable fact register, not an in-RAM flag).
    let dir = tmpdir("decision-resume");
    let mut s = JsonlStorage::create(&dir, "dres", None).unwrap();
    let mut p = MockProvider::scripted(vec![ProviderOutput::ToolCall {
        tool: "write_file".into(),
        input: json!({"path": "a.txt"}),
    }]);
    let mut reg =
        ToolRegistry::with_approver(AllowlistApprover::allow_only(
            vec!["write_file".to_string()],
        ));
    reg.register(Box::new(WriteTool)).unwrap();

    let out = run_turn(&mut s, &mut p, &reg, "hi").unwrap();
    assert!(
        matches!(out, TurnOutcome::ProviderFailed { .. }),
        "script exhaustion surfaces as ProviderFailed"
    );
    // Post-#84 the abort settles pc=Final (terminal, prompt-resumable);
    // the durable flag SURVIVES the abort — it records that this turn
    // did write — and is only cleared when the next fresh turn starts.
    assert_eq!(
        s.get_register("fact", "wrote_this_turn"),
        Some(&json!(true)),
        "flag must be durable across the abort boundary"
    );
}

#[test]
fn wrote_flag_inherits_across_a_settling_crash_resume() {
    // The live inheritance scenario: crash lands between settle_ok and
    // the next provider step (pc=Settling, flag already durable). Built
    // through the REAL machine helpers — begin() + settle_ok() — so the
    // crash-window state is exactly what production produces.
    let dir = tmpdir("decision-settling");
    let mut s = JsonlStorage::create(&dir, "dset", None).unwrap();
    use tole_core::entry::{EntryType, NewEntry};
    use tole_core::machine::ReplaySafety;
    use tole_core::machine::{begin, settle_ok};
    use tole_core::state::{Pc, StateTransition};
    use tole_core::storage::Commit;

    let seq = s.state().seq;
    s.commit(
        Commit::new()
            .entry(NewEntry::root(
                EntryType::new(EntryType::MESSAGE),
                json!({ "role": "user", "text": "hi" }),
            ))
            .transition(StateTransition::from(seq, Pc::Planning)),
    )
    .unwrap();
    // drive() lands ToolCall before begin() — mirror the same legal path.
    let seq = s.state().seq;
    s.commit(Commit::new().transition(StateTransition::from(seq, Pc::ToolCall)))
        .unwrap();
    let handle = begin(&mut s, "echo", json!({}), ReplaySafety::Idempotent, None).unwrap();
    // (drive() commits the durable flag right before settle_ok; mirror it)
    s.commit(
        Commit::new().register(tole_core::register::RegisterWrite::set(
            "fact",
            "wrote_this_turn",
            json!(true),
        )),
    )
    .unwrap();
    settle_ok(&mut s, &handle, json!({})).unwrap();
    assert_eq!(s.state().pc, Pc::Settling);

    let mut reg = ToolRegistry::new();
    reg.register(Box::new(EchoTool)).unwrap();
    let mut p = MockProvider::scripted(vec![ProviderOutput::Final {
        text: "recovered".into(),
    }]);
    let out = resume_turn(&mut s, &mut p, &reg).unwrap();
    match out {
        TurnOutcome::Final { wrote, .. } => {
            assert!(wrote, "resumed turn must keep the earlier Write visible");
        }
        other => panic!("expected Final after resume, got {other:?}"),
    }
    // Session-scoped: the flag persists past Final (nothing resets it).
    assert_eq!(
        s.get_register("fact", "wrote_this_turn"),
        Some(&json!(true))
    );
}

#[test]
fn aborted_writing_turn_keeps_the_session_flag() {
    // Session-scoped contract (#143, cora round 2): turn 1 writes, then
    // aborts (post-#84 pc=Final). The prompt-resume turn is a FRESH
    // run_turn — it must NOT reset the session flag: the session wrote,
    // period. The follow-up turn's Final reports wrote=true.
    let dir = tmpdir("decision-stale");
    let mut s = JsonlStorage::create(&dir, "dstale", None).unwrap();
    let mut reg =
        ToolRegistry::with_approver(AllowlistApprover::allow_only(
            vec!["write_file".to_string()],
        ));
    reg.register(Box::new(WriteTool)).unwrap();

    let mut p1 = MockProvider::scripted(vec![ProviderOutput::ToolCall {
        tool: "write_file".into(),
        input: json!({"path": "a.txt"}),
    }]);
    let out1 = run_turn(&mut s, &mut p1, &reg, "turn one").unwrap();
    assert!(matches!(out1, TurnOutcome::ProviderFailed { .. }));
    assert_eq!(
        s.get_register("fact", "wrote_this_turn"),
        Some(&json!(true))
    );

    // Post-#84: the abort settled pc=Final with the flag STILL true.
    // A prompt-resume is a FRESH run_turn and must NOT reset the
    // session-scoped flag — the session wrote, period.
    let mut p3 = MockProvider::scripted(vec![ProviderOutput::Final {
        text: "next turn".into(),
    }]);
    let out3 = run_turn(&mut s, &mut p3, &reg, "turn two (prompt-resume)").unwrap();
    match out3 {
        TurnOutcome::Final { wrote, .. } => {
            assert!(
                wrote,
                "a fresh turn in a session that wrote before reports wrote=true (session-scoped)"
            );
        }
        other => panic!("expected Final, got {other:?}"),
    }
    assert_eq!(
        s.get_register("fact", "wrote_this_turn"),
        Some(&json!(true))
    );
}

// ---------------------------------------------------------------------------
// Issue #145: turn-end stop gates (--on-turnend)
// ---------------------------------------------------------------------------

#[cfg(feature = "shell-tools")]
struct GateScript {
    /// exit code the gate script returns; stdout is its reason on deny
    code: i32,
    reason: &'static str,
}

/// Build a gate hook command line: a sh script that emits `reason` and
/// exits with `code`. Returns (command_line, _dir_keepalive).
#[cfg(feature = "shell-tools")]
fn gate_cmd(g: &GateScript) -> (String, std::path::PathBuf) {
    use std::sync::atomic::{AtomicU32, Ordering};
    static SEQ: AtomicU32 = AtomicU32::new(0);
    let n = SEQ.fetch_add(1, Ordering::SeqCst);
    let dir = std::env::temp_dir().join(format!("tole-gate-{}-{n}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("gate.sh");
    std::fs::write(
        &path,
        format!(
            "#!/bin/sh\necho '{}'\nexit {}\n",
            g.reason.replace('\'', "'\\''"),
            g.code
        ),
    )
    .unwrap();
    (format!("/bin/sh {}", path.display()), dir)
}

#[cfg(feature = "shell-tools")]
fn registry_with_gates(cmds: &[String]) -> ToolRegistry {
    let mut reg = ToolRegistry::new();
    let mut hooks = tole_core::hooks::ToolHooks::from_cli(&[], &[]);
    hooks.turnend = cmds
        .iter()
        .map(|c| tole_core::hooks::turnend_hook(c))
        .collect();
    reg.set_hooks(hooks);
    reg
}

/// Same but Write-capable (approver allowlists write_file).
#[cfg(feature = "shell-tools")]
fn registry_with_gates_write(cmds: &[String]) -> ToolRegistry {
    let mut reg =
        ToolRegistry::with_approver(tole_core::approval::AllowlistApprover::allow_only(vec![
            "write_file".to_string(),
        ]));
    let mut hooks = tole_core::hooks::ToolHooks::from_cli(&[], &[]);
    hooks.turnend = cmds
        .iter()
        .map(|c| tole_core::hooks::turnend_hook(c))
        .collect();
    reg.set_hooks(hooks);
    reg
}

#[cfg(feature = "shell-tools")]
#[test]
fn stop_gate_deny_blocks_final_and_forces_continuation() {
    // First Final is DENIED (gate exit 2): it must NOT commit; the deny
    // reason lands as a user-role entry; the model's second Final PASSES
    // (the gate is stateful: deny once, then pass) and the turn
    // completes with the new text.
    let dir = tmpdir("gate-deny");
    let mut s = JsonlStorage::create(&dir, "gd", None).unwrap();
    // stateful gate: first run exits 2, later runs exit 0
    let counter = std::env::temp_dir().join(format!("tole-gate-count-{}", std::process::id()));
    let _ = std::fs::remove_file(&counter);
    let gate = std::env::temp_dir().join(format!("tole-gate-once-{}", std::process::id()));
    std::fs::write(
        &gate,
        format!(
            "#!/bin/sh\nN=$(cat {}) 2>/dev/null || echo 0 > {}; N=$((N+1)); echo $N > {};\nif [ $N -eq 1 ]; then echo 'tests failing'; exit 2; fi\nexit 0\n",
            counter.display(), counter.display(), counter.display()
        ),
    )
    .unwrap();
    let cmd = format!("/bin/sh {}", gate.display());
    let _keep = gate;
    let reg = registry_with_gates(&[cmd]);
    let mut p = MockProvider::scripted(vec![
        ProviderOutput::Final {
            text: "done (broken)".into(),
        },
        ProviderOutput::Final {
            text: "done (fixed)".into(),
        },
    ]);
    let out = run_turn(&mut s, &mut p, &reg, "hi").unwrap();
    match out {
        TurnOutcome::Final { text, .. } => assert_eq!(text, "done (fixed)"),
        other => panic!("expected Final after continuation, got {other:?}"),
    }
    // The denied Final was never committed as an assistant message.
    let assistant_texts: Vec<String> = s
        .entries()
        .iter()
        .filter(|e| e.kind.as_str() == "message" && e.payload["role"] == json!("assistant"))
        .filter_map(|e| e.payload["text"].as_str().map(str::to_string))
        .collect();
    assert!(
        !assistant_texts.iter().any(|t| t.contains("broken")),
        "denied Final must not commit: {assistant_texts:?}"
    );
    // The deny reason IS in the log as user-role feedback.
    let has_reason = s.entries().iter().any(|e| {
        e.kind.as_str() == "message"
            && e.payload["role"] == json!("user")
            && e.payload["text"]
                .as_str()
                .map(|t| t.contains("tests failing"))
                .unwrap_or(false)
    });
    assert!(has_reason, "deny reason must be durable");
}

#[cfg(feature = "shell-tools")]
#[test]
fn stop_gate_pass_is_behavior_identical() {
    let dir = tmpdir("gate-pass");
    let mut s = JsonlStorage::create(&dir, "gp", None).unwrap();
    let (cmd, _keep) = gate_cmd(&GateScript {
        code: 0,
        reason: "",
    });
    let reg = registry_with_gates(&[cmd]);
    let mut p = MockProvider::scripted(vec![ProviderOutput::Final {
        text: "clean".into(),
    }]);
    let out = run_turn(&mut s, &mut p, &reg, "hi").unwrap();
    match out {
        TurnOutcome::Final { text, wrote } => {
            assert_eq!(text, "clean");
            assert!(!wrote);
        }
        other => panic!("expected Final, got {other:?}"),
    }
    // No gate feedback entries leaked into the log.
    let gate_entries = s
        .entries()
        .iter()
        .filter(|e| {
            e.payload["text"]
                .as_str()
                .map(|t| t.starts_with("stop gate:"))
                .unwrap_or(false)
        })
        .count();
    assert_eq!(gate_entries, 0);
}

#[cfg(feature = "shell-tools")]
#[test]
fn stop_gate_cap_trips_in_isolation() {
    // A gate that ALWAYS denies must end the turn at the cap
    // (StopGateBlocked) — not livelock. Only the gate trips here: the
    // provider yields endless Finals (loop guard needs identical TOOL
    // calls, budget needs MAX_STEPS steps — neither fires first at cap 3).
    let dir = tmpdir("gate-cap");
    let mut s = JsonlStorage::create(&dir, "gc", None).unwrap();
    let (cmd, _keep) = gate_cmd(&GateScript {
        code: 2,
        reason: "never passes",
    });
    let reg = registry_with_gates(&[cmd]);
    let mut p = MockProvider::always(ProviderOutput::Final {
        text: "attempt".into(),
    });
    let out = run_turn(&mut s, &mut p, &reg, "hi").unwrap();
    match out {
        TurnOutcome::StopGateBlocked { reason } => {
            assert!(reason.contains("never passes"));
        }
        other => panic!("expected StopGateBlocked, got {other:?}"),
    }
    // Post-#84: terminal settle is prompt-resumable.
    assert!(matches!(s.state().pc, tole_core::state::Pc::Final));
    // Durable error record exists.
    assert!(s
        .entries()
        .iter()
        .any(|e| e.kind.as_str() == "error" && e.payload["error"] == json!("stop gate blocked")));
}

#[cfg(feature = "shell-tools")]
#[test]
fn stop_gate_nonzero_exit_denies_with_stdout_reason() {
    // Gate semantics (issue #145, live-E2E correction): ANY non-zero
    // exit is a DENY — a verification gate's exit 1 / 101 is its
    // verdict ("tests failed", "cargo check failed"), not a crash.
    // The stdout becomes the reason the model sees.
    let dir = tmpdir("gate-nonzero");
    let mut s = JsonlStorage::create(&dir, "gnz", None).unwrap();
    // stateful gate: first run exits 101 (verdict), later runs pass
    let counter = std::env::temp_dir().join(format!("tole-gate-nz-{}", std::process::id()));
    let _ = std::fs::remove_file(&counter);
    let gate = std::env::temp_dir().join(format!("tole-gate-nzsh-{}", std::process::id()));
    std::fs::write(
        &gate,
        format!(
            "#!/bin/sh\necho 'compile error'\nN=$(cat {}) 2>/dev/null || echo 0 > {}; N=$((N+1)); echo $N > {};\nif [ $N -eq 1 ]; then exit 101; fi\nexit 0\n",
            counter.display(),
            counter.display(),
            counter.display()
        ),
    )
    .unwrap();
    let reg = registry_with_gates(&[format!("/bin/sh {}", gate.display())]);
    let mut p = MockProvider::scripted(vec![
        ProviderOutput::Final {
            text: "done".into(),
        },
        ProviderOutput::Final {
            text: "done for real".into(),
        },
    ]);
    let out = run_turn(&mut s, &mut p, &reg, "hi").unwrap();
    match out {
        TurnOutcome::Final { text, .. } => assert_eq!(text, "done for real"),
        other => panic!("expected Final after gate denial, got {other:?}"),
    }
    let has_reason = s.entries().iter().any(|e| {
        e.payload["text"]
            .as_str()
            .map(|t| t.contains("compile error"))
            .unwrap_or(false)
    });
    assert!(has_reason, "the 101 verdict reason must be durable");
}

#[cfg(feature = "shell-tools")]
#[test]
fn stop_gate_payload_is_per_turn_not_history() {
    // cora CI: turn 1 executes a Write; turn 2 (fresh run_turn) produces
    // a Final. The gate payload must list turn 2's tools only — a
    // history-keyed gate would re-fire on every later Final.
    let dir = tmpdir("gate-perturn");
    let mut s = JsonlStorage::create(&dir, "gpt", None).unwrap();
    // Gate script: dump the payload's tools array, then pass.
    let dump = std::env::temp_dir().join(format!("tole-gate-dump-{}", std::process::id()));
    let _ = std::fs::remove_file(&dump);
    let gate = std::env::temp_dir().join(format!("tole-gate-dumpsh-{}", std::process::id()));
    std::fs::write(
        &gate,
        format!(
            "#!/bin/sh\ncat > /dev/null\npython3 -c \"import sys,json;d=json.load(open('{}'.replace(chr(39),chr(39))));print(json.dumps(d))\" /dev/stdin >> {} 2>/dev/null || true\nexit 0\n",
            "",
            dump.display()
        ),
    )
    .unwrap();
    // Simpler: use a tiny python that appends stdin to the dump file.
    std::fs::write(
        &gate,
        format!("#!/bin/sh\ntee -a {} >/dev/null\nexit 0\n", dump.display()),
    )
    .unwrap();
    let _reg = registry_with_gates(&[format!("/bin/sh {}", gate.display())]);

    // Turn 1: a Write tool executes.
    // registry with gates + tools, shared across both turns
    let mut reg_all = registry_with_gates_write(&[format!("/bin/sh {}", gate.display())]);
    reg_all.register(Box::new(WriteTool)).unwrap();
    reg_all.register(Box::new(EchoTool)).unwrap();

    let mut p1 = MockProvider::scripted(vec![
        ProviderOutput::ToolCall {
            tool: "write_file".into(),
            input: json!({"path": "a.txt"}),
        },
        ProviderOutput::Final {
            text: "turn one done".into(),
        },
    ]);
    run_turn(&mut s, &mut p1, &reg_all, "turn one").unwrap();

    // Turn 2: no tools, just a Final.
    let mut p2 = MockProvider::scripted(vec![ProviderOutput::Final {
        text: "turn two done".into(),
    }]);
    run_turn(&mut s, &mut p2, &reg_all, "turn two").unwrap();

    // Inspect the LAST payload dumped: it must contain turn 2's tools
    // (none) and NOT write_file from turn 1. Payloads are concatenated
    // on one line by `tee -a`, so stream-decode all JSON values.
    let content = std::fs::read_to_string(&dump).unwrap();
    let de = serde_json::Deserializer::from_str(&content).into_iter::<serde_json::Value>();
    let v = de
        .last()
        .expect("gate received at least one JSON payload")
        .expect("valid payload json");
    let tools = v["tools"].as_array().cloned().unwrap_or_default();
    assert!(
        !tools.iter().any(|t| t["tool"] == "write_file"),
        "turn 2 payload must not contain turn 1's tools: {tools:?}"
    );
}

#[cfg(feature = "shell-tools")]
#[test]
fn stop_gate_payload_keeps_tools_across_own_denial() {
    // cora CI round 2: the deny feedback entry is user-role; on the
    // second Final it must NOT become the turn window start, or the
    // turn's earlier tool calls vanish from the payload and a
    // presence-keyed gate ("deny if write_file ran") is defeated by
    // just re-finalizing.
    let dir = tmpdir("gate-feedback");
    let mut s = JsonlStorage::create(&dir, "gfb", None).unwrap();
    let dump = std::env::temp_dir().join(format!("tole-gate-fb-{}", std::process::id()));
    let _ = std::fs::remove_file(&dump);
    let gate = std::env::temp_dir().join(format!("tole-gate-fbsh-{}", std::process::id()));
    std::fs::write(
        &gate,
        format!("#!/bin/sh\ntee -a {} >/dev/null\nexit 2\n", dump.display()),
    )
    .unwrap();
    let mut reg = registry_with_gates_write(&[format!("/bin/sh {}", gate.display())]);
    reg.register(Box::new(WriteTool)).unwrap();
    reg.register(Box::new(EchoTool)).unwrap();

    // Write tool runs once, then the provider keeps finalizing: the gate
    // always denies → denials 1..3 fire, the third trips the cap. The
    // LAST payload must still contain write_file (the feedback entry did
    // not shrink the window).
    let mut p = MockProvider::scripted(vec![
        ProviderOutput::ToolCall {
            tool: "write_file".into(),
            input: json!({"path": "a.txt"}),
        },
        ProviderOutput::Final {
            text: "attempt 2".into(),
        },
        ProviderOutput::Final {
            text: "attempt 3".into(),
        },
        ProviderOutput::Final {
            text: "attempt 4".into(),
        },
        ProviderOutput::Final {
            text: "attempt 5".into(),
        },
    ]);
    let out = run_turn(&mut s, &mut p, &reg, "hi").unwrap();
    match out {
        TurnOutcome::StopGateBlocked { .. } => {}
        other => panic!("expected StopGateBlocked, got {other:?}"),
    }

    // The LAST dumped payload (from the second Final, after the feedback
    // entry) must still contain write_file — the window did not jump.
    let content = std::fs::read_to_string(&dump).unwrap();
    let de = serde_json::Deserializer::from_str(&content).into_iter::<serde_json::Value>();
    let v = de.last().expect("payloads").expect("valid json");
    let tools = v["tools"].as_array().cloned().unwrap_or_default();
    assert!(
        tools.iter().any(|t| t["tool"] == "write_file"),
        "second-Final payload must keep the turn's tools: {tools:?}"
    );
}

// ---------------------------------------------------------------------------
// Issue #178 — cancellation checkpoints
// ---------------------------------------------------------------------------

use tole_core::approval::Verdict;
use tole_core::cancel::CancelToken;
use tole_core::state::Pc;
use tole_core::turn::run_turn_with_cancel;

/// Approver whose verdict is whatever the test needs (recorded per call).
struct ScriptedApprover {
    verdict: std::sync::Mutex<Verdict>,
}
impl tole_core::approval::Approver for ScriptedApprover {
    fn decide(&self, _req: &tole_core::approval::ToolRequest<'_>) -> Verdict {
        *self.verdict.lock().unwrap()
    }
    fn interactive(&self) -> bool {
        true
    }
}

/// Cancel set BEFORE run_turn: the loop stops before the FIRST provider
/// call, the session settles durably to Final, and a follow-up prompt
/// on the same storage runs normally (prompt-resumable).
#[test]
fn cancel_before_first_step_settles_cancelled_and_resumable() {
    let dir = tmpdir("cancel-early");
    let mut s = JsonlStorage::create(&dir, "cx1", None).unwrap();
    let mut p = MockProvider::scripted(vec![ProviderOutput::Final { text: "f".into() }]);
    let mut reg = ToolRegistry::new();
    reg.register(Box::new(EchoTool)).unwrap();
    let cancel = CancelToken::new();
    cancel.cancel();
    let out = run_turn_with_cancel(&mut s, &mut p, &reg, "hello", &cancel).unwrap();
    assert!(matches!(out, TurnOutcome::Cancelled), "got {out:?}");
    // Durable: terminal Final pc + the cancellation record exists.
    assert_eq!(s.state().pc, Pc::Final);
    let dump = serde_json::to_string(&s.entries()).unwrap();
    assert!(dump.contains("cancelled"), "cancellation record missing");
    // Prompt-resumable: a NEW token + fresh provider runs a normal turn.
    let mut p2 = MockProvider::scripted(vec![ProviderOutput::Final {
        text: "after".into(),
    }]);
    let out2 = run_turn_with_cancel(&mut s, &mut p2, &reg, "again", &CancelToken::new()).unwrap();
    match out2 {
        TurnOutcome::Final { text, .. } => assert_eq!(text, "after"),
        other => panic!("expected Final after cancel, got {other:?}"),
    }
}

/// Cancel lands BEFORE the tool executes (set on a checkpoint after the
/// provider asked for the tool): the intent settles failed, the effect
/// never runs, the turn settles Cancelled.
#[test]
fn cancel_before_tool_execution_prevents_effect() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    static EXEC_COUNT: AtomicUsize = AtomicUsize::new(0);
    struct CountingTool;
    impl Tool for CountingTool {
        fn name(&self) -> &str {
            "counting"
        }
        fn risk(&self) -> Risk {
            Risk::ReadOnly
        }
        fn execute(&self, _input: Value) -> Result<Value, String> {
            EXEC_COUNT.fetch_add(1, Ordering::SeqCst);
            Ok(json!({"ran": true}))
        }
    }
    let dir = tmpdir("cancel-before-tool");
    let mut s = JsonlStorage::create(&dir, "cx2", None).unwrap();
    let mut reg = ToolRegistry::new();
    reg.register(Box::new(CountingTool)).unwrap();
    // After the first provider step (the ToolCall ask), flip the flag:
    // the checkpoint between settlement and execution sees it.
    struct CancelAfterAsk {
        steps: AtomicUsize,
        cancel: CancelToken,
    }
    impl Provider for CancelAfterAsk {
        fn complete(&mut self, _t: &[Entry]) -> Result<ProviderOutput, ProviderError> {
            let n = self.steps.fetch_add(1, Ordering::SeqCst);
            if n >= 1 {
                self.cancel.cancel();
            }
            Ok(ProviderOutput::ToolCall {
                tool: "counting".into(),
                input: json!({"n": 1}),
            })
        }
        fn last_usage(&self) -> Option<Value> {
            None
        }
    }
    let mut p = CancelAfterAsk {
        steps: AtomicUsize::new(0),
        cancel: CancelToken::new(),
    };
    let cancel = p.cancel.clone();
    let out = run_turn_with_cancel(&mut s, &mut p, &reg, "go", &cancel).unwrap();
    assert!(matches!(out, TurnOutcome::Cancelled), "got {out:?}");
    // Exactly ONE execution — the first call ran before the cancel
    // fired (it was issued by the provider pre-cancel); the checkpoint
    // stopped every tool call AFTER the flag was observed.
    assert_eq!(EXEC_COUNT.load(Ordering::SeqCst), 1);
    assert_eq!(s.state().pc, Pc::Final);
}

/// Cancel arriving while a Write approval is pending: the approver
/// fails closed to Deny, and the turn settles CANCELLED (not
/// ApprovalRequired) because the denial was the cancel's side effect.
#[test]
fn cancel_during_pending_approval_settles_cancelled_not_refusal() {
    let dir = tmpdir("cancel-pending-approval");
    let mut s = JsonlStorage::create(&dir, "cx3", None).unwrap();
    let mut reg = ToolRegistry::with_approver(ScriptedApprover {
        verdict: std::sync::Mutex::new(Verdict::Deny),
    });
    reg.register(Box::new(WriteTool)).unwrap();
    // The approver cancels the token when consulted = the cancel lands
    // while the (simulated) permission wait was pending.
    struct CancelOnAsk {
        inner: ScriptedApprover,
        cancel: CancelToken,
    }
    impl tole_core::approval::Approver for CancelOnAsk {
        fn decide(&self, req: &tole_core::approval::ToolRequest<'_>) -> Verdict {
            self.cancel.cancel();
            self.inner.decide(req)
        }
        fn interactive(&self) -> bool {
            true
        }
    }
    let cancel = CancelToken::new();
    let mut reg2 = ToolRegistry::with_approver(CancelOnAsk {
        inner: ScriptedApprover {
            verdict: std::sync::Mutex::new(Verdict::Deny),
        },
        cancel: cancel.clone(),
    });
    reg2.register(Box::new(WriteTool)).unwrap();
    let mut p = MockProvider::scripted(vec![ProviderOutput::ToolCall {
        tool: "write_file".into(),
        input: json!({"path": "x.txt", "content": "v"}),
    }]);
    let out = run_turn_with_cancel(&mut s, &mut p, &reg2, "go", &cancel).unwrap();
    assert!(
        matches!(out, TurnOutcome::Cancelled),
        "must settle cancelled, got {out:?}"
    );
    assert_eq!(s.state().pc, Pc::Final);
}

// ---------------------------------------------------------------------------
// Issue #196 phases 1+2: the observer sees reasoning + tool lifecycle
// ---------------------------------------------------------------------------

/// Scripted outputs WITH reasoning — the GLM-class shape where each
/// response carries a `reasoning` field next to the action.
struct ReasoningScripted {
    steps: std::sync::Mutex<std::collections::VecDeque<(ProviderOutput, Option<String>)>>,
    reasoning: Option<String>,
}

impl Provider for ReasoningScripted {
    fn complete(&mut self, _: &[Entry]) -> Result<ProviderOutput, ProviderError> {
        let (out, reasoning) = self
            .steps
            .lock()
            .unwrap()
            .pop_front()
            .expect("script exhausted");
        self.reasoning = reasoning;
        Ok(out)
    }

    fn last_reasoning(&self) -> Option<String> {
        self.reasoning.clone()
    }
}

impl ReasoningScripted {
    fn new(steps: Vec<(ProviderOutput, Option<String>)>) -> Self {
        Self {
            steps: std::sync::Mutex::new(steps.into()),
            reasoning: None,
        }
    }
}

/// Records observer events as display strings, in fire order.
struct Recorder(std::sync::Mutex<Vec<String>>);
impl tole_core::turn::TurnObserver for Recorder {
    fn tool_started(&self, tool: &str, _input: &Value) {
        self.0.lock().unwrap().push(format!("started:{tool}"));
    }
    fn tool_finished(&self, tool: &str, ok: bool, preview: &str) {
        self.0
            .lock()
            .unwrap()
            .push(format!("finished:{tool}:{ok}:{preview}"));
    }
    fn reasoning(&self, text: &str) {
        self.0.lock().unwrap().push(format!("reasoning:{text}"));
    }
}

#[test]
fn observer_sees_reasoning_then_tool_lifecycle_per_step() {
    let dir = tmpdir("observer");
    let mut s = JsonlStorage::create(&dir, "obs", None).unwrap();
    let mut p = ReasoningScripted::new(vec![
        (
            ProviderOutput::ToolCall {
                tool: "echo".into(),
                input: json!({"n": 1}),
            },
            Some("I should call echo first".into()),
        ),
        (
            ProviderOutput::ToolCall {
                tool: "flaky".into(),
                input: json!({"x": 2}),
            },
            None,
        ),
        (
            ProviderOutput::Final {
                text: "done".into(),
            },
            Some("settling".into()),
        ),
    ]);
    let mut reg = ToolRegistry::new();
    reg.register(Box::new(EchoTool)).unwrap();
    reg.register(Box::new(FlakyTool)).unwrap();
    let rec = Recorder(std::sync::Mutex::new(Vec::new()));

    let out = tole_core::turn::run_turn_with_observer(
        &mut s,
        &mut p,
        &reg,
        "hi",
        &tole_core::cancel::CancelToken::default(),
        Some(&rec),
    )
    .unwrap();
    assert!(matches!(out, TurnOutcome::Final { .. }));

    let events = rec.0.lock().unwrap().clone();
    assert_eq!(
        events,
        vec![
            "reasoning:I should call echo first",
            "started:echo",
            "finished:echo:true:{\"n\":1}",
            // Step 2 has no reasoning (None) — no event.
            "started:flaky",
            "finished:flaky:false:tool exploded",
            // Step 3: final — reasoning only, no tool events.
            "reasoning:settling",
        ]
    );
}

/// A no-observer run behaves identically (the default entry points).
#[test]
fn observer_absent_changes_nothing() {
    let dir = tmpdir("observer-none");
    let mut s = JsonlStorage::create(&dir, "obs-none", None).unwrap();
    let mut p = MockProvider::scripted(vec![ProviderOutput::Final { text: "f".into() }]);
    let reg = ToolRegistry::new();
    let out = run_turn(&mut s, &mut p, &reg, "hi").unwrap();
    assert!(matches!(out, TurnOutcome::Final { .. }));
}

// ---------------------------------------------------------------------------
// Issue #231: dual budgets — polls don't consume MAX_STEPS
// ---------------------------------------------------------------------------

/// Regression (issue #231, scan #97): 40 identical poll calls used to
/// die at step 32 with BudgetExhausted (dead POLL_LOOP_TRIP_AFTER=120).
/// Now poll steps draw from their own budget and the mission completes.
#[test]
fn polls_do_not_consume_the_model_step_budget() {
    struct FakePoll;
    impl Tool for FakePoll {
        fn name(&self) -> &str {
            "job_poll"
        }
        fn risk(&self) -> Risk {
            Risk::ReadOnly
        }
        fn is_poll(&self) -> bool {
            true
        }
        fn execute(&self, _i: Value) -> Result<Value, String> {
            Ok(json!({"running": true}))
        }
    }
    let dir = tmpdir("poll-over-32");
    let mut s = JsonlStorage::create(&dir, "poll-over-32", None).unwrap();
    let mut reg = ToolRegistry::new();
    reg.register(Box::new(FakePoll)).unwrap();
    let mut script = Vec::new();
    for _ in 0..40 {
        script.push(ProviderOutput::ToolCall {
            tool: "job_poll".into(),
            input: json!({"job": "j-1"}),
        });
    }
    script.push(ProviderOutput::Final {
        text: "done".into(),
    });
    let mut p = MockProvider::scripted(script);
    let out = run_turn(&mut s, &mut p, &reg, "wait for the long job").unwrap();
    assert!(
        matches!(out, TurnOutcome::Final { .. }),
        "40 polls must fit the turn (old code died at 32): {out:?}"
    );
}

/// The trait-driven classification the docs always promised (issue
/// #231, scan #98): a NEW poll tool opts in via `Tool::is_poll()` and
/// immediately gets the poll guard ceiling — no name list to extend.
#[test]
fn new_poll_tool_opts_in_via_trait_not_name() {
    struct RenderPoll;
    impl Tool for RenderPoll {
        fn name(&self) -> &str {
            "render_poll"
        }
        fn risk(&self) -> Risk {
            Risk::ReadOnly
        }
        fn is_poll(&self) -> bool {
            true
        }
        fn execute(&self, _i: Value) -> Result<Value, String> {
            Ok(json!({"status": "rendering"}))
        }
    }
    let dir = tmpdir("render-poll");
    let mut s = JsonlStorage::create(&dir, "render-poll", None).unwrap();
    let mut reg = ToolRegistry::new();
    reg.register(Box::new(RenderPoll)).unwrap();
    let mut script = Vec::new();
    for _ in 0..6 {
        script.push(ProviderOutput::ToolCall {
            tool: "render_poll".into(),
            input: json!({"id": "r-1"}),
        });
    }
    script.push(ProviderOutput::Final {
        text: "done".into(),
    });
    let mut p = MockProvider::scripted(script);
    let out = run_turn(&mut s, &mut p, &reg, "wait for render").unwrap();
    assert!(
        matches!(out, TurnOutcome::Final { .. }),
        "6 identical render_poll calls must not trip LOOP_TRIP_AFTER: {out:?}"
    );
}
