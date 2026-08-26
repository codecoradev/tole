//! E2 — state machine: transition legality, seq CAS, effect sandwich,
//! deterministic crash-resume.
//!
//! Determinism proof shape (acceptance criterion 2): run a no-crash
//! reference session, then run the same script with a simulated crash at
//! each sandwich point; after resume, the observable durable state
//! (entries, registers, machine position) must equal the reference.

use cora_agent_core::machine::{
    begin, finish, resume, EffectHandle, ReplaySafety, Resume, PENDING_KEY,
};
use cora_agent_core::state::{Pc, StateTransition};
use cora_agent_core::storage::{Commit, JsonlStorage, Storage, StorageError};
use serde_json::{json, Value};

fn tmpdir(name: &str) -> std::path::PathBuf {
    let d = std::env::temp_dir().join(format!("cora-e2-{}-{}", name, std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

/// Where to simulate the crash, relative to the sandwich.
#[derive(Clone, Copy, PartialEq, Debug)]
enum CrashPoint {
    /// Kill right after `begin` commits (before the effect runs).
    AfterIntent,
    /// Kill while the effect runs (no settlement at all).
    DuringEffect,
    /// Kill after the effect ran, before `settle` commits.
    BeforeSettlement,
    /// No crash — the reference run.
    None,
}

/// The deterministic tool used across all runs. `runs` counts executions
/// so tests can assert exactly-once / at-most-once semantics.
struct CounterTool {
    runs: std::cell::Cell<u32>,
}

impl CounterTool {
    fn execute(&self, input: &Value) -> Value {
        self.runs.set(self.runs.get() + 1);
        json!({ "echo": input["x"], "run": self.runs.get() })
    }
}

/// Drives one full turn: user message → plan → tool (sandwich) → replan →
/// final. Returns the session's observable state for comparison.
fn drive(
    dir: &std::path::Path,
    crash: CrashPoint,
    safety: ReplaySafety,
) -> (Vec<String>, Vec<String>, Pc, u32) {
    let mut s = JsonlStorage::create(dir, "s", None).unwrap();
    // Idle → Planning
    s.commit(Commit::new().transition(StateTransition::from(s.state().seq, Pc::Planning)))
        .unwrap();

    let tool = CounterTool {
        runs: std::cell::Cell::new(0),
    };
    // Planning → ToolCall, then the sandwich.
    s.commit(Commit::new().transition(StateTransition::from(s.state().seq, Pc::ToolCall)))
        .unwrap();

    let handle = begin(&mut s, "counter", json!({"x": 1}), safety, None).unwrap();
    let crashed = matches!(
        crash,
        CrashPoint::AfterIntent | CrashPoint::DuringEffect | CrashPoint::BeforeSettlement
    );
    if !crashed || matches!(crash, CrashPoint::BeforeSettlement) {
        // The effect window: no storage involvement, exactly where a crash
        // is simulated by dropping `s` without settling.
        if !crashed {
            let out = tool.execute(&json!({"x": 1}));
            settle_ok_and_finish(&mut s, &handle, out);
        } else {
            // BeforeSettlement: effect ran, crash before the settlement
            // commit. Drop without settling.
            let _ = tool.execute(&json!({"x": 1}));
            return snapshot(&s, tool.runs.get());
        }
    } else {
        // Crash before/during the effect: drop the session here.
        return snapshot(&s, tool.runs.get());
    }

    // Planning → Final
    s.commit(Commit::new().transition(StateTransition::from(s.state().seq, Pc::Final)))
        .unwrap();
    snapshot(&s, tool.runs.get())
}

fn settle_ok_and_finish(s: &mut JsonlStorage, handle: &EffectHandle, out: Value) {
    cora_agent_core::machine::settle_ok(s, handle, out).unwrap();
    finish(s).unwrap();
}

fn snapshot(s: &JsonlStorage, runs: u32) -> (Vec<String>, Vec<String>, Pc, u32) {
    let kinds: Vec<String> = s
        .entries()
        .iter()
        .map(|e| e.kind.as_str().to_string())
        .collect();
    let regs: Vec<String> = {
        let mut v: Vec<String> = s
            .list_register("pending")
            .into_iter()
            .map(|(k, _)| k.to_string())
            .collect();
        v.sort();
        v
    };
    (kinds, regs, s.state().pc, runs)
}

/// Resumes a crashed session (reopens the file) and finishes the turn to
/// completion, per the replay contract.
fn resume_and_finish(
    dir: &std::path::Path,
    safety: ReplaySafety,
) -> (Vec<String>, Vec<String>, Pc, u32) {
    let mut s = JsonlStorage::open(dir.join("s.jsonl")).unwrap();
    let tool = CounterTool {
        runs: std::cell::Cell::new(0),
    };
    match resume(&mut s).unwrap() {
        Resume::Clean => {
            if s.state().pc == Pc::Settling {
                finish(&mut s).unwrap();
            }
        }
        Resume::ReExecute {
            intent_id,
            tool: tname,
            input,
            safety: contract,
        } => {
            assert_eq!(tname, "counter");
            assert_eq!(contract, safety);
            let handle = EffectHandle { intent_id };
            // Guarded tools verify their guard first; this test tool is
            // pure, so the guard trivially passes.
            let out = tool.execute(&input);
            settle_ok_and_finish(&mut s, &handle, out);
        }
        Resume::Fail { intent_id, reason } => {
            let handle = EffectHandle { intent_id };
            cora_agent_core::machine::settle_err(&mut s, &handle, &reason).unwrap();
        }
    }
    s.commit(Commit::new().transition(StateTransition::from(s.state().seq, Pc::Final)))
        .unwrap();
    snapshot(&s, tool.runs.get())
}

// ---------------------------------------------------------------------------
// Acceptance 1: seq monotonic, stale write rejected (error, not overwrite)
// ---------------------------------------------------------------------------

#[test]
fn stale_seq_is_rejected_not_overwritten() {
    let dir = tmpdir("stale");
    let mut s = JsonlStorage::create(&dir, "st", None).unwrap();
    let observed = s.state().seq;
    s.commit(Commit::new().transition(StateTransition::from(observed, Pc::Planning)))
        .unwrap();
    let err = s
        .commit(Commit::new().transition(StateTransition::from(observed, Pc::ToolCall)))
        .unwrap_err();
    assert!(matches!(err, StorageError::StaleTransition { .. }));
    // Machine untouched by the rejected commit.
    assert_eq!(s.state().pc, Pc::Planning);
}

// ---------------------------------------------------------------------------
// Acceptance 1b: illegal pc transitions are errors, never panics
// ---------------------------------------------------------------------------

#[test]
fn illegal_transitions_error_not_panic() {
    let dir = tmpdir("illegal");
    let mut s = JsonlStorage::create(&dir, "il", None).unwrap();
    // Idle → Executing is not a legal step (no intent persisted).
    let err = s
        .commit(Commit::new().transition(StateTransition::from(s.state().seq, Pc::Executing)))
        .unwrap_err();
    assert!(matches!(err, StorageError::IllegalTransition { .. }));
    // The machine did not move.
    assert_eq!(s.state().pc, Pc::Idle);

    // Legal chain, then a step backwards outside the table.
    s.commit(Commit::new().transition(StateTransition::from(s.state().seq, Pc::Planning)))
        .unwrap();
    let err = s
        .commit(Commit::new().transition(StateTransition::from(s.state().seq, Pc::Idle)))
        .unwrap_err();
    assert!(matches!(err, StorageError::IllegalTransition { .. }));
    assert_eq!(s.state().pc, Pc::Planning);
}

// ---------------------------------------------------------------------------
// Acceptance 2: deterministic replay — crash at each sandwich point
// ---------------------------------------------------------------------------

/// The core determinism test: for a replayable tool, every crash point
/// converges to the reference (no-crash) durable state.
#[test]
fn crash_resume_matches_no_crash_run() {
    for crash in [
        CrashPoint::AfterIntent,
        CrashPoint::DuringEffect,
        CrashPoint::BeforeSettlement,
    ] {
        // Reference (no crash).
        let ref_dir = tmpdir("ref");
        let reference = drive(&ref_dir, CrashPoint::None, ReplaySafety::Idempotent);

        // Crashed run + resume.
        let dir = tmpdir("crash");
        let crashed_state = drive(&dir, crash, ReplaySafety::Idempotent);
        // Sanity: the crash actually happened before settlement.
        assert!(
            crashed_state.2 != Pc::Final,
            "crash point {:?} did not stop before settlement",
            crash
        );
        let resumed = resume_and_finish(&dir, ReplaySafety::Idempotent);

        assert_eq!(
            resumed, reference,
            "resumed state after {:?} must equal the no-crash run",
            crash
        );
    }
}

/// A never-replay tool: after a crash, resume settles the intent as failed.
/// The durable state diverges from the no-crash run *by contract* (error
/// entry instead of a tool_result), never silently.
#[test]
fn never_replay_crash_settles_as_failure() {
    let dir = tmpdir("never");
    let crashed = drive(&dir, CrashPoint::DuringEffect, ReplaySafety::Never);
    assert_ne!(crashed.2, Pc::Final);

    let mut s = JsonlStorage::open(dir.join("s.jsonl")).unwrap();
    match resume(&mut s).unwrap() {
        Resume::Fail { intent_id, reason } => {
            assert!(!reason.is_empty());
            let handle = EffectHandle { intent_id };
            cora_agent_core::machine::settle_err(&mut s, &handle, &reason).unwrap();
        }
        other => panic!("expected Fail, got {other:?}"),
    }
    // Pending cleared, loop back in Planning, and an error entry exists.
    assert!(s.get_register("pending", PENDING_KEY).is_none());
    assert_eq!(s.state().pc, Pc::Planning);
    assert!(s.entries().iter().any(|e| e.kind.as_str() == "error"));
}

/// Effect failure path (settle_err) from a live (non-crashed) run.
#[test]
fn effect_error_settles_and_replans() {
    let dir = tmpdir("err");
    let mut s = JsonlStorage::create(&dir, "er", None).unwrap();
    s.commit(Commit::new().transition(StateTransition::from(s.state().seq, Pc::Planning)))
        .unwrap();
    s.commit(Commit::new().transition(StateTransition::from(s.state().seq, Pc::ToolCall)))
        .unwrap();
    let handle = begin(
        &mut s,
        "flaky",
        json!({"x": 1}),
        ReplaySafety::Idempotent,
        None,
    )
    .unwrap();
    cora_agent_core::machine::settle_err(&mut s, &handle, "boom").unwrap();
    assert!(s.get_register("pending", PENDING_KEY).is_none());
    assert_eq!(s.state().pc, Pc::Planning);
    // Re-plan a new sandwich after the failure: legal per the table.
    s.commit(Commit::new().transition(StateTransition::from(s.state().seq, Pc::ToolCall)))
        .unwrap();
    let h2 = begin(
        &mut s,
        "flaky",
        json!({"x": 2}),
        ReplaySafety::Idempotent,
        None,
    )
    .unwrap();
    let out = json!({"ok": true});
    cora_agent_core::machine::settle_ok(&mut s, &h2, out).unwrap();
    finish(&mut s).unwrap();
    assert_eq!(s.state().pc, Pc::Planning);
}

/// Replay of a compacted file keeps the sandwich machinery working: the
/// snapshot state record must not be treated as an illegal step.
#[test]
fn compacted_file_replays_and_resumes_clean() {
    let dir = tmpdir("compact");
    let mut s = JsonlStorage::create(&dir, "cp", None).unwrap();
    s.commit(Commit::new().transition(StateTransition::from(s.state().seq, Pc::Planning)))
        .unwrap();
    s.commit(Commit::new().transition(StateTransition::from(s.state().seq, Pc::ToolCall)))
        .unwrap();
    let handle = begin(
        &mut s,
        "counter",
        json!({"x": 9}),
        ReplaySafety::Idempotent,
        None,
    )
    .unwrap();
    cora_agent_core::machine::settle_ok(&mut s, &handle, json!({"v": 9})).unwrap();
    s.compact().unwrap();
    drop(s);

    let mut s = JsonlStorage::open(dir.join("cp.jsonl")).unwrap();
    assert_eq!(s.state().pc, Pc::Settling);
    assert!(matches!(resume(&mut s).unwrap(), Resume::Clean));
    finish(&mut s).unwrap();
    assert_eq!(s.state().pc, Pc::Planning);
}

/// A hand-edited file walking outside the transition table is rejected at
/// open (corrupt), enforcing the table on replay too.
#[test]
fn illegal_replayed_transition_is_rejected_at_open() {
    use std::io::Write;
    let dir = tmpdir("tamper");
    let mut s = JsonlStorage::create(&dir, "tp", None).unwrap();
    s.commit(Commit::new().transition(StateTransition::from(s.state().seq, Pc::Planning)))
        .unwrap();
    drop(s);

    // Tamper: append a state record Planning → Idle (illegal).
    let path = dir.join("tp.jsonl");
    let mut f = std::fs::OpenOptions::new()
        .append(true)
        .open(&path)
        .unwrap();
    writeln!(
        f,
        r#"{{"kind":"state","seq":99,"pc":"idle","snapshot":false}}"#
    )
    .unwrap();
    drop(f);

    let err = match JsonlStorage::open(&path) {
        Err(e) => e,
        Ok(_) => panic!("tampered file must be rejected at open"),
    };
    assert!(matches!(err, StorageError::IllegalTransition { .. }));
}

// ---------------------------------------------------------------------------
// begin() legality guard
// ---------------------------------------------------------------------------

#[test]
fn begin_from_idle_is_illegal() {
    let dir = tmpdir("begin-idle");
    let mut s = JsonlStorage::create(&dir, "bi", None).unwrap();
    let err = begin(&mut s, "t", json!({}), ReplaySafety::Idempotent, None).unwrap_err();
    assert!(matches!(err, StorageError::IllegalTransition { .. }));
}
