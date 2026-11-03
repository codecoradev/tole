//! Sequential read-only tool-call batches (#480): one model response with
//! several tool calls runs them in model order as consecutive effect
//! sandwiches, inside ONE model step.

use serde_json::{json, Value};
use std::collections::VecDeque;
use std::sync::Mutex;
use tole_core::approval::AllowlistApprover;
use tole_core::cancel::CancelToken;
use tole_core::entry::Entry;
use tole_core::machine::{begin_batch_member, settle_ok, ReplaySafety};
use tole_core::mock::MockProvider;
use tole_core::provider::{Provider, ProviderError, ProviderOutput};
use tole_core::storage::{JsonlStorage, Storage};
use tole_core::tool::{Risk, Tool, ToolRegistry};
use tole_core::turn::{
    resume_turn, run_turn, run_turn_with_observer, TurnObserver, TurnOutcome, LOOP_TRIP_AFTER,
    MAX_STEPS,
};

struct Echo;
impl Tool for Echo {
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

struct Flaky;
impl Tool for Flaky {
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

struct Writer;
impl Tool for Writer {
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

/// Scripted responses: each is (first call or final, extra calls).
struct Batcher {
    script: VecDeque<(ProviderOutput, Vec<(String, Value)>)>,
    extras: Vec<(String, Value)>,
    calls: usize,
}
impl Batcher {
    fn new(script: Vec<(ProviderOutput, Vec<(String, Value)>)>) -> Self {
        Self {
            script: script.into(),
            extras: Vec::new(),
            calls: 0,
        }
    }
}
impl Provider for Batcher {
    fn complete(&mut self, _t: &[Entry]) -> Result<ProviderOutput, ProviderError> {
        self.calls += 1;
        let (out, extras) = self
            .script
            .pop_front()
            .ok_or_else(|| ProviderError("script exhausted".into()))?;
        self.extras = extras;
        Ok(out)
    }
    fn last_extra_tool_calls(&self) -> Vec<(String, Value)> {
        self.extras.clone()
    }
}

fn call(tool: &str, input: Value) -> ProviderOutput {
    ProviderOutput::ToolCall {
        tool: tool.into(),
        input,
    }
}
fn fin(text: &str) -> ProviderOutput {
    ProviderOutput::Final { text: text.into() }
}
fn extra(tool: &str, input: Value) -> (String, Value) {
    (tool.into(), input)
}

fn tmpdir(name: &str) -> std::path::PathBuf {
    static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let d = std::env::temp_dir().join(format!("tole-batch-{name}-{}-{n:x}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn registry() -> ToolRegistry {
    let mut r =
        ToolRegistry::with_approver(AllowlistApprover::allow_only(
            vec!["write_file".to_string()],
        ));
    r.register(Box::new(Echo)).unwrap();
    r.register(Box::new(Flaky)).unwrap();
    r.register(Box::new(Writer)).unwrap();
    r
}

fn kinds(s: &JsonlStorage, kind: &str) -> Vec<Entry> {
    s.entries()
        .iter()
        .filter(|e| e.kind.as_str() == kind)
        .cloned()
        .collect()
}

#[test]
fn a_read_only_batch_runs_in_order_inside_one_step() {
    let mut s = JsonlStorage::create(tmpdir("ro"), "ro", None).unwrap();
    let mut p = Batcher::new(vec![
        (
            call("echo", json!({"n": 1})),
            vec![
                extra("echo", json!({"n": 2})),
                extra("echo", json!({"n": 3})),
            ],
        ),
        (fin("done"), vec![]),
    ]);
    let out = run_turn(&mut s, &mut p, &registry(), "go").unwrap();
    assert!(matches!(out, TurnOutcome::Final { .. }), "{out:?}");
    assert_eq!(p.calls, 2, "one provider step for the batch + the final");
    let intents = kinds(&s, "intent");
    let order: Vec<_> = intents
        .iter()
        .map(|e| e.payload["input"]["n"].clone())
        .collect();
    assert_eq!(order, vec![json!(1), json!(2), json!(3)]);
    // First intent opens the assistant message; the rest continue it.
    let cont: Vec<_> = intents
        .iter()
        .map(|e| e.payload["batch"] == json!(true))
        .collect();
    assert_eq!(cont, vec![false, true, true]);
    assert_eq!(kinds(&s, "tool_result").len(), 3, "each call settled");
    for e in &intents {
        assert_eq!(e.payload["replay"], json!("idempotent"));
    }
}

#[test]
fn a_batch_with_any_write_falls_back_to_the_first_call_only() {
    let mut s = JsonlStorage::create(tmpdir("mixed"), "mixed", None).unwrap();
    let mut p = Batcher::new(vec![
        (
            call("echo", json!({"n": 1})),
            vec![
                extra("write_file", json!({"path": "x"})),
                extra("echo", json!({"n": 3})),
            ],
        ),
        (fin("done"), vec![]),
    ]);
    run_turn(&mut s, &mut p, &registry(), "go").unwrap();
    let intents = kinds(&s, "intent");
    assert_eq!(intents.len(), 1, "only the first call ran");
    assert_eq!(intents[0].payload["tool"], json!("echo"));
}

#[test]
fn a_batch_with_an_unknown_tool_falls_back_to_the_first_call_only() {
    let mut s = JsonlStorage::create(tmpdir("unk"), "unk", None).unwrap();
    let mut p = Batcher::new(vec![
        (
            call("echo", json!({"n": 1})),
            vec![extra("nope", json!({}))],
        ),
        (fin("done"), vec![]),
    ]);
    run_turn(&mut s, &mut p, &registry(), "go").unwrap();
    assert_eq!(kinds(&s, "intent").len(), 1);
}

#[test]
fn a_failure_stops_the_batch_and_stubs_the_rest() {
    let mut s = JsonlStorage::create(tmpdir("fail"), "fail", None).unwrap();
    let mut p = Batcher::new(vec![
        (
            call("echo", json!({"n": 1})),
            vec![extra("flaky", json!({})), extra("echo", json!({"n": 3}))],
        ),
        (fin("done"), vec![]),
    ]);
    let out = run_turn(&mut s, &mut p, &registry(), "go").unwrap();
    assert!(matches!(out, TurnOutcome::Final { .. }), "{out:?}");
    let intents = kinds(&s, "intent");
    assert_eq!(intents.len(), 3, "every call has an intent");
    let errs = kinds(&s, "error");
    assert_eq!(errs.len(), 2, "failed call + stub, both settled as errors");
    assert!(errs[0].payload["error"]
        .as_str()
        .unwrap()
        .contains("exploded"));
    let stub = errs[1].payload["error"].as_str().unwrap();
    assert!(stub.contains("not executed"), "{stub}");
    assert_eq!(errs[1].parent_id.as_deref(), Some(intents[2].id.as_str()));
    assert_eq!(kinds(&s, "tool_result").len(), 1);
}

#[test]
fn identical_calls_inside_a_batch_feed_the_loop_guard() {
    let mut s = JsonlStorage::create(tmpdir("loop"), "loop", None).unwrap();
    let same = json!({"n": 1});
    let mut p = Batcher::new(vec![(
        call("echo", same.clone()),
        vec![extra("echo", same.clone()), extra("echo", same)],
    )]);
    let out = run_turn(&mut s, &mut p, &registry(), "go").unwrap();
    match out {
        TurnOutcome::LoopDetected { count, .. } => assert_eq!(count, LOOP_TRIP_AFTER),
        other => panic!("expected LoopDetected, got {other:?}"),
    }
}

#[test]
fn a_batch_costs_one_step_of_the_budget() {
    let mut s = JsonlStorage::create(tmpdir("budget"), "budget", None).unwrap();
    let mut script = Vec::new();
    // MAX_STEPS - 1 batched tool steps (3 calls each, all distinct), then
    // the final answer on the last allowed step.
    for i in 0..MAX_STEPS - 1 {
        script.push((
            call("echo", json!({"i": i, "k": 0})),
            vec![
                extra("echo", json!({"i": i, "k": 1})),
                extra("echo", json!({"i": i, "k": 2})),
            ],
        ));
    }
    script.push((fin("done"), vec![]));
    let mut p = Batcher::new(script);
    let out = run_turn(&mut s, &mut p, &registry(), "go").unwrap();
    assert!(matches!(out, TurnOutcome::Final { .. }), "{out:?}");
    assert_eq!(kinds(&s, "intent").len(), (MAX_STEPS - 1) * 3);
}

#[derive(Default)]
struct Rec(Mutex<Vec<String>>);
impl TurnObserver for Rec {
    fn tool_started(&self, tool: &str, input: &Value) {
        self.0.lock().unwrap().push(format!("start {tool} {input}"));
    }
    fn tool_finished(&self, tool: &str, ok: bool, _p: &str) {
        self.0.lock().unwrap().push(format!("done {tool} {ok}"));
    }
}

#[test]
fn the_observer_sees_each_call_as_its_own_started_finished_pair() {
    let mut s = JsonlStorage::create(tmpdir("obs"), "obs", None).unwrap();
    let mut p = Batcher::new(vec![
        (
            call("echo", json!({"n": 1})),
            vec![extra("echo", json!({"n": 2}))],
        ),
        (fin("done"), vec![]),
    ]);
    let rec = Rec::default();
    run_turn_with_observer(
        &mut s,
        &mut p,
        &registry(),
        "go",
        &CancelToken::default(),
        Some(&rec),
    )
    .unwrap();
    assert_eq!(
        *rec.0.lock().unwrap(),
        vec![
            r#"start echo {"n":1}"#,
            "done echo true",
            r#"start echo {"n":2}"#,
            "done echo true",
        ]
    );
}

#[test]
fn a_crash_mid_batch_resumes_the_pending_call_and_the_model_re_asks_the_rest() {
    let mut s = JsonlStorage::create(tmpdir("crash"), "crash", None).unwrap();
    use tole_core::entry::{EntryType, NewEntry};
    use tole_core::machine::begin;
    use tole_core::state::{Pc, StateTransition};
    use tole_core::storage::Commit;
    let seq = s.state().seq;
    s.commit(
        Commit::new()
            .entry(NewEntry::root(
                EntryType::new(EntryType::MESSAGE),
                json!({ "role": "user", "text": "go" }),
            ))
            .transition(StateTransition::from(seq, Pc::Planning)),
    )
    .unwrap();
    let seq = s.state().seq;
    s.commit(Commit::new().transition(StateTransition::from(seq, Pc::ToolCall)))
        .unwrap();
    let h1 = begin(
        &mut s,
        "echo",
        json!({"n": 1}),
        ReplaySafety::Idempotent,
        None,
    )
    .unwrap();
    settle_ok(&mut s, &h1, json!({"n": 1})).unwrap();
    tole_core::machine::finish(&mut s).unwrap();
    let seq = s.state().seq;
    s.commit(Commit::new().transition(StateTransition::from(seq, Pc::ToolCall)))
        .unwrap();
    // Crash window: call 2 of the batch is durable but unsettled.
    begin_batch_member(
        &mut s,
        "echo",
        json!({"n": 2}),
        ReplaySafety::Idempotent,
        None,
    )
    .unwrap();

    let mut p = MockProvider::scripted(vec![fin("recovered")]);
    let out = resume_turn(&mut s, &mut p, &registry()).unwrap();
    assert!(matches!(out, TurnOutcome::Final { .. }), "{out:?}");
    assert_eq!(
        kinds(&s, "tool_result").len(),
        2,
        "pending call re-executed"
    );
    let intents = kinds(&s, "intent");
    assert_eq!(intents[1].payload["batch"], json!(true));
}
