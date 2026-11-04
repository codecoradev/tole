//! #476: a denied approval (or pre-tool hook deny) parks the turn in
//! Planning, but the denial is visible to the model's next request and
//! `run_turn` (the `resume <id> "<prompt>"` path) settles the parked turn
//! instead of refusing it. Bare `resume_turn` still replans in place.

use serde_json::{json, Value};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use tole_core::approval::{Approver, ToolRequest, Verdict};
use tole_core::entry::Entry;
use tole_core::machine::{begin, ReplaySafety};
use tole_core::mock::MockProvider;
use tole_core::provider::{Provider, ProviderError, ProviderOutput};
use tole_core::state::{Pc, StateTransition};
use tole_core::storage::{Commit, JsonlStorage, Storage};
use tole_core::tool::{Risk, Tool, ToolRegistry};
use tole_core::turn::{resume_turn, run_turn, TurnOutcome};

struct Deny;
impl Approver for Deny {
    fn decide(&self, _req: &ToolRequest<'_>) -> Verdict {
        Verdict::Deny
    }
    fn interactive(&self) -> bool {
        true
    }
}

struct Probe(Arc<AtomicUsize>);
impl Tool for Probe {
    fn name(&self) -> &str {
        "w"
    }
    fn risk(&self) -> Risk {
        Risk::Write
    }
    fn execute(&self, _input: Value) -> Result<Value, String> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Ok(json!({"ran": true}))
    }
}

/// Scripted provider that records every transcript it is handed.
struct Rec {
    inner: MockProvider,
    seen: Arc<Mutex<Vec<Vec<Entry>>>>,
}
impl Provider for Rec {
    fn complete(&mut self, t: &[Entry]) -> Result<ProviderOutput, ProviderError> {
        self.seen.lock().unwrap().push(t.to_vec());
        self.inner.complete(t)
    }
}

fn rec(script: Vec<ProviderOutput>) -> (Rec, Arc<Mutex<Vec<Vec<Entry>>>>) {
    let seen = Arc::new(Mutex::new(Vec::new()));
    (
        Rec {
            inner: MockProvider::scripted(script),
            seen: seen.clone(),
        },
        seen,
    )
}

fn call() -> ProviderOutput {
    ProviderOutput::ToolCall {
        tool: "w".into(),
        input: json!({"n": 1}),
    }
}

fn fin(t: &str) -> ProviderOutput {
    ProviderOutput::Final { text: t.into() }
}

fn rig() -> (ToolRegistry, Arc<AtomicUsize>) {
    let runs = Arc::new(AtomicUsize::new(0));
    let mut reg = ToolRegistry::with_approver(Deny);
    reg.register(Box::new(Probe(runs.clone()))).unwrap();
    (reg, runs)
}

fn tmp(tag: &str) -> JsonlStorage {
    let d = std::env::temp_dir().join(format!("tole-denial-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    JsonlStorage::create(&d, "s", None).unwrap()
}

/// The user-role messages of a transcript, as text.
fn user_texts(t: &[Entry]) -> Vec<String> {
    t.iter()
        .filter(|e| e.kind.as_str() == "message" && e.payload["role"] == json!("user"))
        .map(|e| e.payload["text"].as_str().unwrap().to_string())
        .collect()
}

fn denied_session(tag: &str) -> (JsonlStorage, ToolRegistry, Arc<AtomicUsize>) {
    let (reg, runs) = rig();
    let mut s = tmp(tag);
    let mut p = MockProvider::scripted(vec![call(), fin("unreachable")]);
    let out = run_turn(&mut s, &mut p, &reg, "go").unwrap();
    assert!(
        matches!(out, TurnOutcome::ApprovalRequired { .. }),
        "{out:?}"
    );
    assert_eq!(
        s.state().pc,
        Pc::Planning,
        "interactive recovery stays parked"
    );
    (s, reg, runs)
}

#[test]
fn prompt_after_denial_settles_the_parked_turn_and_runs_a_new_one() {
    let (mut s, reg, runs) = denied_session("prompt");
    let (mut p, seen) = rec(vec![fin("replanned")]);
    let out = run_turn(&mut s, &mut p, &reg, "continue without that tool").unwrap();
    assert!(
        matches!(&out, TurnOutcome::Final { text, .. } if text == "replanned"),
        "{out:?}"
    );
    assert_eq!(s.state().pc, Pc::Final);
    assert_eq!(runs.load(Ordering::SeqCst), 0, "a denied effect never runs");
    // The new request carries the denial, then the new instruction.
    let users = user_texts(&seen.lock().unwrap()[0]);
    assert_eq!(users.len(), 3, "{users:?}");
    assert!(
        users[1].contains("`w`") && users[1].contains("denied"),
        "{users:?}"
    );
    assert_eq!(users[2], "continue without that tool");
}

#[test]
fn bare_resume_after_denial_sees_the_denial() {
    let (mut s, reg, runs) = denied_session("bare");
    let (mut p, seen) = rec(vec![fin("replanned")]);
    let out = resume_turn(&mut s, &mut p, &reg).unwrap();
    assert!(matches!(out, TurnOutcome::Final { .. }), "{out:?}");
    assert_eq!(runs.load(Ordering::SeqCst), 0);
    let users = user_texts(&seen.lock().unwrap()[0]);
    assert_eq!(users.len(), 2, "{users:?}");
    assert!(
        users[1].contains("`w`") && users[1].contains("denied"),
        "{users:?}"
    );
}

#[test]
fn denial_note_does_not_open_a_turn() {
    let (s, _reg, _runs) = denied_session("turns");
    assert_eq!(tole_core::transcript::turn_count(s.entries()), 1);
}

#[test]
fn replay_denied_says_the_outcome_is_unknown_and_a_prompt_continues() {
    let (reg, runs) = rig();
    let mut s = tmp("replay");
    let seq = s.state().seq;
    s.commit(
        Commit::new()
            .entry(tole_core::entry::NewEntry::root(
                tole_core::entry::EntryType::new(tole_core::entry::EntryType::MESSAGE),
                json!({ "role": "user", "text": "do it" }),
            ))
            .transition(StateTransition::from(seq, Pc::Planning)),
    )
    .unwrap();
    let seq = s.state().seq;
    s.commit(Commit::new().transition(StateTransition::from(seq, Pc::ToolCall)))
        .unwrap();
    begin(&mut s, "w", json!({"n": 1}), ReplaySafety::Guarded, None).unwrap();

    let mut p = MockProvider::scripted(vec![]);
    let out = resume_turn(&mut s, &mut p, &reg).unwrap();
    assert!(
        matches!(out, TurnOutcome::ApprovalRequired { .. }),
        "{out:?}"
    );
    assert_eq!(runs.load(Ordering::SeqCst), 0);
    let settled: Vec<String> = s
        .entries()
        .iter()
        .filter(|e| e.kind.as_str() == "error" && e.payload["ok"] == json!(false))
        .map(|e| e.payload["error"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(settled.len(), 1);
    assert!(
        settled[0].contains("outcome is UNKNOWN")
            && settled[0].contains("may have run before the interruption")
            && settled[0].contains("check state before retrying"),
        "{settled:?}"
    );

    let (mut p, seen) = rec(vec![fin("checked")]);
    let out = run_turn(&mut s, &mut p, &reg, "check the state").unwrap();
    assert!(matches!(out, TurnOutcome::Final { .. }), "{out:?}");
    let users = user_texts(&seen.lock().unwrap()[0]);
    assert!(users.iter().any(|u| u.contains("UNKNOWN")), "{users:?}");
    assert_eq!(users.last().unwrap(), "check the state");
}

#[test]
fn a_plain_parked_turn_is_still_refused() {
    let (reg, _runs) = rig();
    let mut s = tmp("plain");
    let seq = s.state().seq;
    s.commit(Commit::new().transition(StateTransition::from(seq, Pc::Planning)))
        .unwrap();
    let mut p = MockProvider::scripted(vec![fin("x")]);
    let err = run_turn(&mut s, &mut p, &reg, "hi").unwrap_err();
    assert!(
        err.to_string().contains("requires pc Idle or Final"),
        "{err}"
    );
}
