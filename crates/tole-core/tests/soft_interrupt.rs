//! #377 — soft interrupts: steer a running turn without cancelling it.
//!
//! The mock provider/tool play the "second client": they push into the
//! session's `CancelToken` interrupt queue at the moments a real client
//! could (while a tool runs, while the provider call is in flight).

use serde_json::{json, Value};
use std::collections::VecDeque;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use tole_core::approval::AllowlistApprover;
use tole_core::cancel::CancelToken;
use tole_core::entry::Entry;
use tole_core::provider::{Provider, ProviderError, ProviderOutput};
use tole_core::storage::{JsonlStorage, Storage};
use tole_core::tool::{Risk, Tool, ToolRegistry};
use tole_core::turn::{run_turn_with_cancel, TurnOutcome};

fn tmpdir(name: &str) -> std::path::PathBuf {
    static SEQ: AtomicUsize = AtomicUsize::new(0);
    let n = SEQ.fetch_add(1, Ordering::Relaxed);
    let d = std::env::temp_dir().join(format!("tole-int-{name}-{}-{n}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

/// ReadOnly tool that counts executions and, when armed, pushes an
/// interrupt while it "runs".
struct WorkTool {
    runs: Arc<AtomicUsize>,
    push_while_running: Option<(CancelToken, &'static str, bool)>,
}

impl Tool for WorkTool {
    fn name(&self) -> &str {
        "work"
    }
    fn risk(&self) -> Risk {
        Risk::ReadOnly
    }
    fn execute(&self, _input: Value) -> Result<Value, String> {
        self.runs.fetch_add(1, Ordering::SeqCst);
        if let Some((tok, text, urgent)) = &self.push_while_running {
            tok.interrupts().push(text, *urgent).unwrap();
        }
        Ok(json!({"done": true}))
    }
}

fn registry(tool: WorkTool) -> ToolRegistry {
    let mut r = ToolRegistry::with_approver(AllowlistApprover::allow_only(vec![]));
    r.register(Box::new(tool)).unwrap();
    r
}

type Hook = Box<dyn FnMut(usize)>;

/// Scripted provider that records each transcript it was shown and can
/// run a hook at the start of a call (a client pushing mid-call).
struct Steered {
    outputs: VecDeque<ProviderOutput>,
    hook: Option<Hook>,
    seen: Vec<Vec<Entry>>,
}

impl Provider for Steered {
    fn complete(&mut self, transcript: &[Entry]) -> Result<ProviderOutput, ProviderError> {
        let n = self.seen.len();
        self.seen.push(transcript.to_vec());
        if let Some(h) = self.hook.as_mut() {
            h(n);
        }
        self.outputs
            .pop_front()
            .ok_or_else(|| ProviderError("script exhausted".into()))
    }
}

fn call_work() -> ProviderOutput {
    ProviderOutput::ToolCall {
        tool: "work".into(),
        input: json!({}),
    }
}

fn final_text(t: &str) -> ProviderOutput {
    ProviderOutput::Final { text: t.into() }
}

fn is_interrupt(e: &Entry) -> bool {
    e.kind.as_str() == "message" && e.payload["interrupt"] == json!(true)
}

/// No `tool_use` without its `tool_result`: every intent has one settlement.
fn assert_no_orphan_intents(entries: &[Entry]) {
    let intents = entries
        .iter()
        .filter(|e| e.kind.as_str() == "intent")
        .count();
    let results = entries
        .iter()
        .filter(|e| matches!(e.kind.as_str(), "tool_result" | "error") && e.parent_id.is_some())
        .filter(|e| {
            entries
                .iter()
                .any(|i| i.kind.as_str() == "intent" && Some(&i.id) == e.parent_id.as_ref())
        })
        .count();
    assert_eq!(intents, results, "an intent was left without a settlement");
}

#[test]
fn normal_interrupt_is_delivered_after_the_tool_settles_and_before_the_next_call() {
    let dir = tmpdir("safe-point");
    let mut s = JsonlStorage::create(&dir, "s", None).unwrap();
    let cancel = CancelToken::default();
    let runs = Arc::new(AtomicUsize::new(0));
    let reg = registry(WorkTool {
        runs: Arc::clone(&runs),
        push_while_running: Some((cancel.clone(), "focus on the tests", false)),
    });
    let mut p = Steered {
        outputs: VecDeque::from([call_work(), final_text("done")]),
        hook: None,
        seen: vec![],
    };
    let out = run_turn_with_cancel(&mut s, &mut p, &reg, "go", &cancel).unwrap();
    assert!(matches!(out, TurnOutcome::Final { .. }), "{out:?}");
    assert_eq!(runs.load(Ordering::SeqCst), 1);

    // The second provider call sees: user, intent, tool_result, interrupt.
    let second = &p.seen[1];
    let kinds: Vec<&str> = second.iter().map(|e| e.kind.as_str()).collect();
    let at = second
        .iter()
        .position(is_interrupt)
        .expect("interrupt visible");
    assert!(kinds[..at].contains(&"tool_result"), "{kinds:?}");
    assert_eq!(at, second.len() - 1, "delivered right before the call");

    // Attributed, and not a turn-opening prompt.
    let e = &second[at];
    assert_eq!(e.payload["role"], json!("user"));
    assert_eq!(e.payload["urgent"], json!(false));
    let text = e.payload["text"].as_str().unwrap();
    assert!(
        text.contains("owner interrupt") && text.contains("focus on the tests"),
        "{text}"
    );
    assert!(cancel.interrupts().is_empty());
    assert_no_orphan_intents(s.entries());
}

#[test]
fn urgent_interrupt_withdraws_an_unstarted_tool_call_without_orphaning_it() {
    let dir = tmpdir("urgent");
    let mut s = JsonlStorage::create(&dir, "u", None).unwrap();
    let cancel = CancelToken::default();
    let runs = Arc::new(AtomicUsize::new(0));
    let reg = registry(WorkTool {
        runs: Arc::clone(&runs),
        push_while_running: None,
    });
    let c = cancel.clone();
    let mut p = Steered {
        outputs: VecDeque::from([call_work(), final_text("changed course")]),
        // The client interrupts while the first provider call is in flight.
        hook: Some(Box::new(move |n| {
            if n == 0 {
                c.interrupts().push("stop, do X instead", true).unwrap();
            }
        })),
        seen: vec![],
    };
    let out = run_turn_with_cancel(&mut s, &mut p, &reg, "go", &cancel).unwrap();
    assert!(
        matches!(out, TurnOutcome::Final { ref text, .. } if text == "changed course"),
        "{out:?}"
    );
    assert_eq!(
        runs.load(Ordering::SeqCst),
        0,
        "the withdrawn tool never ran"
    );
    let entries = s.entries();
    assert!(
        entries.iter().all(|e| e.kind.as_str() != "intent"),
        "no intent was written for the withdrawn call"
    );
    assert_no_orphan_intents(entries);
    // The drop is recorded, not silent.
    assert!(entries.iter().any(|e| e.kind.as_str() == "error"
        && e.payload["error"] == json!("superseded by owner interrupt")
        && e.payload["detail"].as_str().unwrap().contains("work")));
    // And the model saw the steering on its next call.
    assert!(p.seen[1].iter().any(is_interrupt));
    assert!(p.seen[1].iter().any(|e| e.payload["urgent"] == json!(true)));
}

#[test]
fn a_normal_interrupt_does_not_withdraw_a_tool_call() {
    let dir = tmpdir("normal-keeps-tool");
    let mut s = JsonlStorage::create(&dir, "n", None).unwrap();
    let cancel = CancelToken::default();
    let runs = Arc::new(AtomicUsize::new(0));
    let reg = registry(WorkTool {
        runs: Arc::clone(&runs),
        push_while_running: None,
    });
    let c = cancel.clone();
    let mut p = Steered {
        outputs: VecDeque::from([call_work(), final_text("done")]),
        hook: Some(Box::new(move |n| {
            if n == 0 {
                c.interrupts().push("by the way, prefer Y", false).unwrap();
            }
        })),
        seen: vec![],
    };
    let out = run_turn_with_cancel(&mut s, &mut p, &reg, "go", &cancel).unwrap();
    assert!(matches!(out, TurnOutcome::Final { .. }));
    assert_eq!(runs.load(Ordering::SeqCst), 1, "the call still ran");
    // Delivered at the next safe point, after the tool settled.
    let second = &p.seen[1];
    let result_at = second
        .iter()
        .position(|e| e.kind.as_str() == "tool_result")
        .unwrap();
    let int_at = second.iter().position(is_interrupt).unwrap();
    assert!(int_at > result_at);
    assert_no_orphan_intents(s.entries());
}

#[test]
fn a_final_answer_is_superseded_by_a_pending_interrupt() {
    let dir = tmpdir("final");
    let mut s = JsonlStorage::create(&dir, "f", None).unwrap();
    let cancel = CancelToken::default();
    let reg = registry(WorkTool {
        runs: Arc::new(AtomicUsize::new(0)),
        push_while_running: None,
    });
    let c = cancel.clone();
    let mut p = Steered {
        outputs: VecDeque::from([final_text("stale answer"), final_text("steered answer")]),
        hook: Some(Box::new(move |n| {
            if n == 0 {
                c.interrupts().push("also cover Z", false).unwrap();
            }
        })),
        seen: vec![],
    };
    let out = run_turn_with_cancel(&mut s, &mut p, &reg, "go", &cancel).unwrap();
    assert!(
        matches!(out, TurnOutcome::Final { ref text, .. } if text == "steered answer"),
        "{out:?}"
    );
    let entries = s.entries();
    assert!(
        !entries
            .iter()
            .any(|e| e.payload["text"] == json!("stale answer")),
        "the withdrawn answer was never committed"
    );
    assert!(entries
        .iter()
        .any(|e| e.payload["error"] == json!("superseded by owner interrupt")));
    assert!(entries.iter().any(is_interrupt));
}

#[test]
fn an_interrupt_queued_before_the_turn_lands_after_the_prompt() {
    let dir = tmpdir("queued");
    let mut s = JsonlStorage::create(&dir, "q", None).unwrap();
    let cancel = CancelToken::default();
    cancel.interrupts().push("queued steering", false).unwrap();
    let reg = registry(WorkTool {
        runs: Arc::new(AtomicUsize::new(0)),
        push_while_running: None,
    });
    let mut p = Steered {
        outputs: VecDeque::from([final_text("ok")]),
        hook: None,
        seen: vec![],
    };
    run_turn_with_cancel(&mut s, &mut p, &reg, "go", &cancel).unwrap();
    let first = &p.seen[0];
    let user_at = first
        .iter()
        .position(|e| e.payload["text"] == json!("go"))
        .unwrap();
    let int_at = first.iter().position(is_interrupt).unwrap();
    assert!(int_at > user_at, "the original prompt still comes first");
}

#[test]
fn without_interrupts_nothing_changes() {
    let dir = tmpdir("none");
    let mut s = JsonlStorage::create(&dir, "z", None).unwrap();
    let cancel = CancelToken::default();
    let reg = registry(WorkTool {
        runs: Arc::new(AtomicUsize::new(0)),
        push_while_running: None,
    });
    let mut p = Steered {
        outputs: VecDeque::from([call_work(), final_text("done")]),
        hook: None,
        seen: vec![],
    };
    run_turn_with_cancel(&mut s, &mut p, &reg, "go", &cancel).unwrap();
    assert!(!s.entries().iter().any(is_interrupt));
    assert!(!s.entries().iter().any(|e| e.kind.as_str() == "error"));
}

#[test]
fn a_notice_is_delivered_attributed_but_never_withdraws_output() {
    // A tool call in flight is NOT withdrawn by a notice, a final answer is
    // NOT superseded by one, and the entry says where it came from.
    let dir = tmpdir("notice");
    let mut s = JsonlStorage::create(&dir, "n", None).unwrap();
    let cancel = CancelToken::default();
    let runs = Arc::new(AtomicUsize::new(0));
    let reg = registry(WorkTool {
        runs: Arc::clone(&runs),
        push_while_running: None,
    });
    let c = cancel.clone();
    let mut p = Steered {
        outputs: VecDeque::from([call_work(), final_text("done")]),
        hook: Some(Box::new(move |n| {
            // Pushed while the first call is in flight and again while the
            // final answer is in flight.
            c.interrupts()
                .push_notice("child agent b changed a.rs")
                .unwrap();
            let _ = n;
        })),
        seen: vec![],
    };
    let out = run_turn_with_cancel(&mut s, &mut p, &reg, "go", &cancel).unwrap();
    assert!(
        matches!(out, TurnOutcome::Final { ref text, .. } if text == "done"),
        "{out:?}"
    );
    assert_eq!(
        runs.load(Ordering::SeqCst),
        1,
        "the tool call was not withdrawn"
    );
    let entries = s.entries();
    assert!(
        !entries
            .iter()
            .any(|e| e.payload["error"] == json!("superseded by owner interrupt")),
        "a notice never supersedes output"
    );
    let notice = entries
        .iter()
        .find(|e| is_interrupt(e))
        .expect("notice delivered");
    assert_eq!(notice.payload["source"], json!("notice"));
    assert_eq!(notice.payload["urgent"], json!(false));
    let text = notice.payload["text"].as_str().unwrap();
    assert!(
        text.contains("sibling notice") && text.contains("child agent b changed a.rs"),
        "{text}"
    );
    assert_no_orphan_intents(entries);
}
