//! #1230: `settle_interrupted` closes a turn a crash cut off, append-only,
//! with no model call and no tool run, whatever the replay contract of an
//! open intent, and leaves a settled or denial-parked session alone.

#[path = "support/test_dir.rs"]
mod test_dir;
use serde_json::{json, Value};
use test_dir::TestDir;
use tole_core::entry::{Entry, EntryType, NewEntry};
use tole_core::machine::{begin, settle_ok, ReplaySafety};
use tole_core::mock::MockProvider;
use tole_core::openai::{OpenAiConfig, OpenAiProvider};
use tole_core::provider::{Provider, ProviderError, ProviderOutput};
use tole_core::state::{Pc, StateTransition};
use tole_core::storage::{Commit, JsonlStorage, Storage};
use tole_core::tool::ToolRegistry;
use tole_core::turn::{run_turn, settle_interrupted, transcript::DENIAL_NOTE};

fn session(tag: &str) -> (JsonlStorage, TestDir) {
    let d = TestDir::new(&format!("tole-settle-{tag}"));
    (JsonlStorage::create(&d, "s", None).unwrap(), d)
}

fn go(s: &mut JsonlStorage, to: Pc) {
    let seq = s.state().seq;
    s.commit(Commit::new().transition(StateTransition::from(seq, to)))
        .unwrap();
}

/// A turn cut off after the user message: pc Planning.
fn planning(s: &mut JsonlStorage, payload: Value) {
    let seq = s.state().seq;
    let msg = NewEntry::root(EntryType::new(EntryType::MESSAGE), payload);
    let t = StateTransition::from(seq, Pc::Planning);
    s.commit(Commit::new().entry(msg).transition(t)).unwrap();
}

fn user(s: &mut JsonlStorage) {
    planning(s, json!({"role": "user", "text": "write it"}));
}

/// Cut off inside a tool step: the intent is durable, pc Executing.
fn open_intent(s: &mut JsonlStorage, safety: ReplaySafety) {
    user(s);
    go(s, Pc::ToolCall);
    let parent = s.entries()[0].id.clone();
    begin(s, "w", json!({"n": 1}), safety, Some(&parent)).unwrap();
}

struct Rec(MockProvider, Vec<Vec<Entry>>);
impl Provider for Rec {
    fn complete(&mut self, t: &[Entry]) -> Result<ProviderOutput, ProviderError> {
        self.1.push(t.to_vec());
        self.0.complete(t)
    }
}

/// The next prompt on `s`, and the transcript the model got for it.
fn next_prompt(s: &mut JsonlStorage) -> Vec<Entry> {
    let mut p = Rec(
        MockProvider::scripted(vec![ProviderOutput::Final { text: "ok".into() }]),
        vec![],
    );
    run_turn(s, &mut p, &ToolRegistry::new(), "again").unwrap();
    p.1.remove(0)
}

fn kinds(s: &JsonlStorage) -> Vec<String> {
    let k = |e: &Entry| e.kind.as_str().to_string();
    s.entries().iter().map(k).collect()
}

fn settled(s: &mut JsonlStorage) {
    assert!(settle_interrupted(s).unwrap());
    assert_eq!(s.state().pc, Pc::Final);
    assert!(s
        .get_register("pending", tole_core::machine::PENDING_KEY)
        .is_none());
    let seqs: Vec<u64> = s.entries().iter().map(|e| e.seq).collect();
    assert!(seqs.windows(2).all(|w| w[0] < w[1]), "{seqs:?}");
    assert!(!settle_interrupted(s).unwrap(), "a second call is a no-op");
}

#[test]
fn a_cut_off_planning_turn_is_settled_and_the_next_prompt_sees_the_message() {
    let (mut s, _d) = session("planning");
    user(&mut s);
    settled(&mut s);
    assert_eq!(kinds(&s), ["message", "error"]);
    assert_eq!(s.entries()[1].payload["error"], "interrupted");
    let seen = next_prompt(&mut s);
    let texts: Vec<_> = seen
        .iter()
        .filter_map(|e| e.payload["text"].as_str())
        .collect();
    assert_eq!(texts, ["write it", "again"]);
}

#[test]
fn an_open_intent_is_failed_never_re_run_for_every_replay_contract() {
    for safety in [
        ReplaySafety::Never,
        ReplaySafety::Idempotent,
        ReplaySafety::Guarded,
    ] {
        let (mut s, _d) = session(safety.as_str());
        open_intent(&mut s, safety);
        assert_eq!(s.state().pc, Pc::Executing);
        settled(&mut s);
        assert_eq!(
            kinds(&s),
            ["message", "intent", "error", "error"],
            "{safety:?}"
        );
        let e = &s.entries()[2];
        assert_eq!(e.parent_id.as_deref(), Some(s.entries()[1].id.as_str()));
        assert!(e.payload["error"].as_str().unwrap().contains("UNKNOWN"));
        assert!(!kinds(&s).contains(&"tool_result".to_string()));
        // The model's next request carries the cut-off call and its failure.
        let wire = OpenAiProvider::new(OpenAiConfig::new("http://x", "m", "k"))
            .request_body(&next_prompt(&mut s))
            .to_string();
        assert!(
            wire.contains("write it") && wire.contains("UNKNOWN"),
            "{wire}"
        );
    }
}

#[test]
fn every_other_cut_off_register_is_settled() {
    for pc in [Pc::ToolCall, Pc::AwaitingApproval] {
        let (mut s, _d) = session(&format!("{pc:?}"));
        user(&mut s);
        go(&mut s, Pc::ToolCall);
        if pc == Pc::AwaitingApproval {
            go(&mut s, pc);
        }
        settled(&mut s);
    }
    // Settled but not yet finished: the sandwich closes, the turn is failed.
    let (mut s, _d) = session("settling");
    open_intent(&mut s, ReplaySafety::Guarded);
    let h = tole_core::machine::EffectHandle {
        intent_id: s.entries()[1].id.clone(),
    };
    settle_ok(&mut s, &h, json!({"ok": true})).unwrap();
    assert_eq!(s.state().pc, Pc::Settling);
    settled(&mut s);
    assert_eq!(kinds(&s), ["message", "intent", "tool_result", "error"]);
}

#[test]
fn a_settled_or_fresh_session_is_not_written() {
    let (mut s, _d) = session("idle");
    assert!(!settle_interrupted(&mut s).unwrap());
    assert_eq!(s.state().seq, 0);
    let mut p = MockProvider::scripted(vec![ProviderOutput::Final { text: "hi".into() }]);
    run_turn(&mut s, &mut p, &ToolRegistry::new(), "x").unwrap();
    let seq = s.state().seq;
    assert_eq!(s.state().pc, Pc::Final);
    assert!(!settle_interrupted(&mut s).unwrap());
    assert_eq!(s.state().seq, seq);
}

#[test]
fn a_turn_parked_after_a_denial_is_left_for_the_next_prompt() {
    let (mut s, _d) = session("denial");
    user(&mut s);
    let note = json!({"role": "user", "text": "[tole] denied", DENIAL_NOTE: true});
    s.commit(Commit::new().entry(NewEntry::root(EntryType::new(EntryType::MESSAGE), note)))
        .unwrap();
    let before = (s.state().seq, kinds(&s));
    assert!(!settle_interrupted(&mut s).unwrap());
    assert_eq!((s.state().seq, kinds(&s)), before, "nothing written");
    assert_eq!(s.state().pc, Pc::Planning);
    // The existing rule still settles it, the denial note stays visible.
    let seen = next_prompt(&mut s);
    assert!(seen.iter().any(|e| e.payload[DENIAL_NOTE] == json!(true)));
    assert_eq!(s.state().pc, Pc::Final);
}
