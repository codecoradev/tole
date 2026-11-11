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
    drop(s); // a writable open takes the session lock
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

/// docs/perf.md: fold of the 100k-record ledger fixture (#578). Unoptimized
/// CI build; observed far below (see docs/perf.md for the numbers).
const LEDGER_FOLD_BUDGET_MS: u128 = 30_000;

/// Automation ledger (#578): fold of a 100k-record log and `ready` over
/// 10k jobs. Both must stay linear-ish; same regression-gate spirit as
/// the turn scenarios (docs/perf.md).
#[test]
fn ledger_fold_and_ready_under_budget() {
    use tole_core::ledger::{
        CatchUp, Delivery, DeliveryStatus, Done, Fire, Job, Lease, Ledger, LedgerStore,
        MemoryStore, Record, RunStatus,
    };
    const JOBS: usize = 10_000;
    const RUNS: usize = 30_000; // x3 records (fire, done, delivery)
    const T: i64 = 1_767_225_600;
    let mut store = MemoryStore::default();
    let mut put = |r: Record| {
        let mut line = r.to_line().into_bytes();
        line.push(b'\n');
        store.append(&line).unwrap();
    };
    for i in 0..JOBS {
        put(Record::Job(Job {
            id: format!("job-{i}"),
            schedule: "*/5 * * * *".into(),
            timezone: None,
            catch_up: CatchUp::Skip,
            grace_minutes: 0,
            paused: false,
            removed: false,
            at: T,
            extra: [("prompt".to_string(), "summarize the inbox".into())]
                .into_iter()
                .collect(),
        }));
    }
    for n in 0..RUNS {
        let job_id = format!("job-{}", n % JOBS);
        let at = T + 300 * (1 + (n / JOBS) as i64);
        let fire_id = format!("{job_id}@{at}/1.1");
        put(Record::Fire(Fire {
            fire_id: fire_id.clone(),
            job_id: job_id.clone(),
            scheduled_for: at,
            claimed_at: at,
            lease: Lease {
                pid: 1,
                start_time: 1,
            },
        }));
        put(Record::Done(Done {
            fire_id: fire_id.clone(),
            job_id,
            status: RunStatus::Ok,
            session_id: Some(format!("s-{n}")),
            reason: None,
            usage: None,
            at,
        }));
        put(Record::Delivery(Delivery {
            fire_id,
            target: "session".into(),
            attempt: 1,
            status: DeliveryStatus::Sent,
            error: None,
            at,
        }));
    }
    let start = Instant::now();
    let mut l = Ledger::open(store.clone()).unwrap();
    let fold = start.elapsed();
    let start = Instant::now();
    let due = l.ready(T + 4 * 300).unwrap();
    let ready = start.elapsed();
    println!(
        "ledger: fold of {} records in {fold:?}, ready over {JOBS} jobs in {ready:?}",
        JOBS + 3 * RUNS
    );
    assert_eq!(due.len(), JOBS, "every job owes the T+20min occurrence");
    assert!(due
        .iter()
        .all(|d| d.fire == Some(T + 1_200) && d.missed.is_none()));
    assert!(
        fold.as_millis() < LEDGER_FOLD_BUDGET_MS && ready.as_millis() < TURN_BUDGET_MS,
        "fold {fold:?} (budget {LEDGER_FOLD_BUDGET_MS}ms), ready {ready:?} (budget {TURN_BUDGET_MS}ms)"
    );
}
