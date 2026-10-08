//! E9 benchmark (#9): single-turn wall time through the REAL turn loop
//! and JSONL storage with a MockProvider (no network) — the harness's
//! own overhead, which is what the release must keep under a documented
//! ceiling. Results are recorded in docs/perf.md.
//!
//! This is a THRESHOLD test, not a microbenchmark: it fails when the
//! harness regresses past the documented budget (CI-enforced), and
//! docs/perf.md is the human-readable record.

use serde_json::json;
use std::time::Instant;
use tole_core::approval::AllowlistApprover;
use tole_core::mock::MockProvider;
use tole_core::provider::ProviderOutput;
use tole_core::storage::{JsonlStorage, Storage};
use tole_core::tool::{Risk, Tool, ToolRegistry};
use tole_core::turn::{run_turn, TurnOutcome};

/// Documented ceiling (docs/perf.md): one full turn — N scripted tool
/// calls through the real effect sandwich + JSONL commits — must
/// complete in under this budget on a dev laptop. Generous by design:
/// it is a regression gate, not a race.
const TURN_BUDGET_MS: u128 = 2_000;

struct EchoTool;
impl Tool for EchoTool {
    fn name(&self) -> &str {
        "echo"
    }
    fn risk(&self) -> Risk {
        Risk::ReadOnly
    }
    fn describe(&self, _input: &serde_json::Value) -> String {
        "echo".into()
    }
    fn execute(&self, input: serde_json::Value) -> Result<serde_json::Value, String> {
        Ok(input)
    }
}

fn tmpdir(tag: &str) -> std::path::PathBuf {
    let d = std::env::temp_dir().join(format!("tole-perf-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

#[test]
fn single_turn_text_only_under_budget() {
    let dir = tmpdir("text");
    let mut s = JsonlStorage::create(&dir, "perf-text", None).unwrap();
    let reg = ToolRegistry::new();
    let mut p = MockProvider::scripted(vec![ProviderOutput::Final {
        text: "done".into(),
    }]);
    let start = Instant::now();
    let out = run_turn(&mut s, &mut p, &reg, "hello").unwrap();
    let elapsed = start.elapsed();
    assert!(matches!(out, TurnOutcome::Final { .. }));
    assert!(
        elapsed.as_millis() < TURN_BUDGET_MS,
        "text-only turn took {elapsed:?} (budget {TURN_BUDGET_MS}ms) — harness regression, update docs/perf.md if intentional"
    );
}

#[test]
fn single_turn_tool_chain_under_budget() {
    let dir = tmpdir("chain");
    let mut s = JsonlStorage::create(&dir, "perf-chain", None).unwrap();
    let mut reg = ToolRegistry::with_approver(AllowlistApprover::allow_only(vec![]));
    reg.register(Box::new(EchoTool)).unwrap();
    // A 10-call chain: each call runs the full intent→effect→settle
    // sandwich with its own JSONL commit — the durability tax per step,
    // measured end-to-end.
    let mut script = Vec::new();
    for i in 0..10 {
        script.push(ProviderOutput::ToolCall {
            tool: "echo".into(),
            input: json!({ "n": i }),
        });
    }
    script.push(ProviderOutput::Final {
        text: "done".into(),
    });
    let mut p = MockProvider::scripted(script);
    let start = Instant::now();
    let out = run_turn(&mut s, &mut p, &reg, "chain").unwrap();
    let elapsed = start.elapsed();
    assert!(matches!(out, TurnOutcome::Final { .. }));
    assert!(
        elapsed.as_millis() < TURN_BUDGET_MS,
        "10-call tool chain took {elapsed:?} (budget {TURN_BUDGET_MS}ms) — harness regression, update docs/perf.md if intentional"
    );
    // The durability record really has the chain.
    assert!(
        s.entries().len() >= 20,
        "expected a durable entry per sandwich half"
    );
}

#[test]
fn resume_replay_under_budget() {
    // Crash-resume replays the WHOLE log; the per-entry replay cost is
    // the number that decides whether long sessions stay practical.
    let dir = tmpdir("replay");
    let mut s = JsonlStorage::create(&dir, "perf-replay", None).unwrap();
    let mut reg = ToolRegistry::with_approver(AllowlistApprover::allow_only(vec![]));
    reg.register(Box::new(EchoTool)).unwrap();
    // Stay below the turn loop's MAX_STEPS (32) so the turn completes.
    const CALLS: usize = 25;
    let mut script = Vec::new();
    for i in 0..CALLS {
        script.push(ProviderOutput::ToolCall {
            tool: "echo".into(),
            input: json!({ "n": i }),
        });
    }
    script.push(ProviderOutput::Final {
        text: "done".into(),
    });
    let mut p = MockProvider::scripted(script);
    let out = run_turn(&mut s, &mut p, &reg, "many").unwrap();
    assert!(
        matches!(out, TurnOutcome::Final { .. }),
        "replay fixture must complete, got {out:?} — otherwise the gate is vacuous"
    );
    let written = s.entries().len();
    // Anti-vacuity: every call must have really executed (an intent and
    // a tool_result each), not aborted on the first unknown tool.
    let results = s
        .entries()
        .iter()
        .filter(|e| e.kind.as_str() == "tool_result")
        .count();
    assert_eq!(results, CALLS, "replay fixture must execute all tool calls");
    assert!(
        written >= 2 * CALLS,
        "expected >= {} entries, got {written}",
        2 * CALLS
    );
    let start = Instant::now();
    let reopened = JsonlStorage::open(dir.join("perf-replay.jsonl")).unwrap();
    let elapsed = start.elapsed();
    assert!(
        elapsed.as_millis() < TURN_BUDGET_MS,
        "replay of {} entries took {elapsed:?} (budget {TURN_BUDGET_MS}ms)",
        reopened.entries().len()
    );
    assert_eq!(
        reopened.entries().len(),
        written,
        "reopen must replay every entry that was written"
    );
}
