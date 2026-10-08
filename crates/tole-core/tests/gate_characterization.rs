//! Characterization tests for the tool-call authorization paths
//! (issue #303, PR 1 of 3 of the gate refactor).
//!
//! These pin what the code does TODAY — check order, outcomes and the
//! exact durable error strings — for the two turn-loop paths:
//!
//! 1. FRESH: `drive` handling a provider `ToolCall` (via `run_turn*`).
//! 2. REPLAY: `resume_turn` re-executing a crash-interrupted intent.
//!
//! (The MCP server path is pinned in `mcp_server.rs`'s test module,
//! because `execute_checked` is private.)
//!
//! Feathers-style: the expectations record observed behavior, not a
//! requirement. A later PR that deliberately changes behavior must
//! update the matching test in the same change. Everything here is
//! greppable via the `gate_char_` prefix.

use serde_json::{json, Value};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use tole_core::approval::{Approver, ToolRequest, Verdict};
use tole_core::cancel::CancelToken;
use tole_core::entry::{EntryType, NewEntry};
use tole_core::machine::{begin, ReplaySafety};
use tole_core::mock::MockProvider;
use tole_core::provider::ProviderOutput;
use tole_core::state::{Pc, StateTransition};
use tole_core::storage::{Commit, JsonlStorage, Storage};
use tole_core::tool::{Risk, Tool, ToolRegistry};
use tole_core::turn::{
    resume_turn, resume_turn_with_cancel, run_turn, run_turn_with_cancel, TurnOutcome,
};

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

fn tmpdir(name: &str) -> std::path::PathBuf {
    static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = SEQ.fetch_add(1, Ordering::Relaxed);
    let d = std::env::temp_dir().join(format!("tole-gatechar-{name}-{}-{n:x}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

/// Tool with a fixed risk that counts executions.
struct Probe {
    name: &'static str,
    risk: Risk,
    runs: Arc<AtomicUsize>,
}
impl Tool for Probe {
    fn name(&self) -> &str {
        self.name
    }
    fn risk(&self) -> Risk {
        self.risk
    }
    fn execute(&self, input: Value) -> Result<Value, String> {
        self.runs.fetch_add(1, Ordering::SeqCst);
        Ok(json!({ "ran": input }))
    }
}

/// Approver that records every consultation (tool, risk) and returns a
/// fixed verdict. `interactive` so Destructive tools can register.
struct Rec {
    verdict: Verdict,
    seen: Arc<Mutex<Vec<(String, &'static str)>>>,
    /// Optional token to cancel when consulted (cancel-while-pending).
    cancel_on_ask: Option<CancelToken>,
}
impl Approver for Rec {
    fn decide(&self, req: &ToolRequest<'_>) -> Verdict {
        self.seen
            .lock()
            .unwrap()
            .push((req.tool.to_string(), req.risk.as_str()));
        if let Some(c) = &self.cancel_on_ask {
            c.cancel();
        }
        self.verdict
    }
    fn interactive(&self) -> bool {
        true
    }
}

struct Rig {
    reg: ToolRegistry,
    runs: Arc<AtomicUsize>,
    seen: Arc<Mutex<Vec<(String, &'static str)>>>,
}

fn rig_with(verdict: Verdict, cancel: Option<CancelToken>, tools: &[(&'static str, Risk)]) -> Rig {
    let runs = Arc::new(AtomicUsize::new(0));
    let seen = Arc::new(Mutex::new(Vec::new()));
    let mut reg = ToolRegistry::with_approver(Rec {
        verdict,
        seen: seen.clone(),
        cancel_on_ask: cancel,
    });
    for (name, risk) in tools {
        reg.register(Box::new(Probe {
            name,
            risk: *risk,
            runs: runs.clone(),
        }))
        .unwrap();
    }
    Rig { reg, runs, seen }
}

fn rig(verdict: Verdict, tools: &[(&'static str, Risk)]) -> Rig {
    rig_with(verdict, None, tools)
}

impl Rig {
    fn consulted(&self) -> usize {
        self.seen.lock().unwrap().len()
    }
    fn runs(&self) -> usize {
        self.runs.load(Ordering::SeqCst)
    }
}

/// Turn-level durable errors (`append_turn_error`): (error, detail).
fn turn_errors(s: &JsonlStorage) -> Vec<(String, String)> {
    s.entries()
        .iter()
        .filter(|e| e.kind.as_str() == "error" && e.payload.get("detail").is_some())
        .map(|e| {
            (
                e.payload["error"].as_str().unwrap().to_string(),
                e.payload["detail"].as_str().unwrap().to_string(),
            )
        })
        .collect()
}

/// Sandwich settlements as failed (`settle_err`): the error message.
fn settle_errors(s: &JsonlStorage) -> Vec<String> {
    s.entries()
        .iter()
        .filter(|e| e.kind.as_str() == "error" && e.payload["ok"] == json!(false))
        .map(|e| e.payload["error"].as_str().unwrap().to_string())
        .collect()
}

fn count_kind(s: &JsonlStorage, kind: &str) -> usize {
    s.entries()
        .iter()
        .filter(|e| e.kind.as_str() == kind)
        .count()
}

fn wrote_flag(s: &JsonlStorage) -> Option<Value> {
    s.get_register("fact", "wrote_this_turn").cloned()
}

fn call(tool: &str) -> ProviderOutput {
    ProviderOutput::ToolCall {
        tool: tool.into(),
        input: json!({"n": 1}),
    }
}

fn final_out(text: &str) -> ProviderOutput {
    ProviderOutput::Final { text: text.into() }
}

/// Run one fresh turn: [ToolCall(tool), Final("done")].
fn fresh(r: &Rig, tool: &str, tag: &str) -> (TurnOutcome, JsonlStorage) {
    let dir = tmpdir(tag);
    let mut s = JsonlStorage::create(&dir, "s", None).unwrap();
    let mut p = MockProvider::scripted(vec![call(tool), final_out("done")]);
    let out = run_turn(&mut s, &mut p, &r.reg, "go").unwrap();
    (out, s)
}

/// Seed a crash-interrupted session: user message, Planning, ToolCall,
/// then an intent left PENDING (pc Executing). Returns (storage, intent id).
fn seed_pending(
    tag: &str,
    tool: &str,
    input: Value,
    safety: ReplaySafety,
) -> (JsonlStorage, String) {
    let dir = tmpdir(tag);
    let mut s = JsonlStorage::create(&dir, "s", None).unwrap();
    let seq = s.state().seq;
    s.commit(
        Commit::new()
            .entry(NewEntry::root(
                EntryType::new(EntryType::MESSAGE),
                json!({ "role": "user", "text": "do it" }),
            ))
            .transition(StateTransition::from(seq, Pc::Planning)),
    )
    .unwrap();
    let seq = s.state().seq;
    s.commit(Commit::new().transition(StateTransition::from(seq, Pc::ToolCall)))
        .unwrap();
    let handle = begin(&mut s, tool, input, safety, None).unwrap();
    assert_eq!(s.state().pc, Pc::Executing);
    (s, handle.intent_id)
}

fn replay(r: &Rig, s: &mut JsonlStorage, script: Vec<ProviderOutput>) -> TurnOutcome {
    let mut p = MockProvider::scripted(script);
    resume_turn(s, &mut p, &r.reg).unwrap()
}

// ---------------------------------------------------------------------------
// Shell-tools-only: pre/post hook scripts
// ---------------------------------------------------------------------------

#[cfg(feature = "shell-tools")]
mod hooks {
    use super::*;
    use tole_core::hooks::{ProcessHook, ToolHooks};

    /// A hook command that touches `marker` then exits `code` printing
    /// `reason`. Returns (command line, marker path).
    pub fn hook(tag: &str, code: i32, reason: &str) -> (String, std::path::PathBuf) {
        let dir = tmpdir(tag);
        let marker = dir.join("ran");
        let script = dir.join("hook.sh");
        std::fs::write(
            &script,
            format!(
                "#!/bin/sh\necho x >> {}\necho '{}'\nexit {code}\n",
                marker.display(),
                reason
            ),
        )
        .unwrap();
        (format!("/bin/sh {}", script.display()), marker)
    }

    pub fn set_pre(r: &mut Rig, cmd: &str) {
        let mut h = ToolHooks::from_cli(&[], &[]);
        h.pre = vec![ProcessHook::new(cmd)];
        r.reg.set_hooks(h);
    }

    pub fn set_post(r: &mut Rig, cmd: &str) {
        let mut h = ToolHooks::from_cli(&[], &[]);
        h.post = vec![ProcessHook::new(cmd)];
        r.reg.set_hooks(h);
    }
}

// ===========================================================================
// 1. FRESH path (`drive`)
// ===========================================================================

#[test]
fn gate_char_fresh_readonly_runs_without_approver_consultation() {
    // Deny-everything approver: a ReadOnly tool must not even reach it.
    let r = rig(Verdict::Deny, &[("r", Risk::ReadOnly)]);
    let (out, s) = fresh(&r, "r", "fresh-ro");
    assert!(
        matches!(out, TurnOutcome::Final { wrote: false, .. }),
        "{out:?}"
    );
    assert_eq!(r.consulted(), 0);
    assert_eq!(r.runs(), 1);
    assert_eq!(
        wrote_flag(&s),
        None,
        "ReadOnly must not set wrote_this_turn"
    );
    assert!(turn_errors(&s).is_empty());
}

#[cfg(feature = "shell-tools")]
#[test]
fn gate_char_fresh_readonly_skips_pre_and_post_hooks() {
    let mut r = rig(Verdict::Deny, &[("r", Risk::ReadOnly)]);
    let (pre, pre_marker) = hooks::hook("fresh-ro-pre", 2, "nope");
    hooks::set_pre(&mut r, &pre);
    let (out, _s) = fresh(&r, "r", "fresh-ro-hook");
    assert!(matches!(out, TurnOutcome::Final { .. }), "{out:?}");
    assert_eq!(r.runs(), 1);
    assert!(!pre_marker.exists(), "pre-hook must not run for ReadOnly");

    let mut r2 = rig(Verdict::Deny, &[("r", Risk::ReadOnly)]);
    let (post, post_marker) = hooks::hook("fresh-ro-post", 0, "");
    hooks::set_post(&mut r2, &post);
    let (out, _s) = fresh(&r2, "r", "fresh-ro-post2");
    assert!(matches!(out, TurnOutcome::Final { .. }), "{out:?}");
    assert!(
        !post_marker.exists(),
        "post-hook must not run for ReadOnly (zero-overhead contract)"
    );
}

#[test]
fn gate_char_fresh_write_allow_executes_and_sets_wrote_fact() {
    let r = rig(Verdict::Allow, &[("w", Risk::Write)]);
    let (out, s) = fresh(&r, "w", "fresh-w-allow");
    match out {
        TurnOutcome::Final { text, wrote } => {
            assert_eq!(text, "done");
            assert!(wrote);
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(r.consulted(), 1);
    assert_eq!(r.seen.lock().unwrap()[0], ("w".to_string(), "Write"));
    assert_eq!(r.runs(), 1);
    assert_eq!(wrote_flag(&s), Some(json!(true)));
    assert_eq!(count_kind(&s, "intent"), 1);
    assert_eq!(count_kind(&s, "tool_result"), 1);
    assert!(turn_errors(&s).is_empty());
    // Write intents are recorded Guarded (replay contract derives from risk).
    let intent = s
        .entries()
        .iter()
        .find(|e| e.kind.as_str() == "intent")
        .unwrap();
    assert_eq!(intent.payload["tool"], json!("w"));
}

#[test]
fn gate_char_fresh_write_deny_ends_approval_required_with_durable_record() {
    let r = rig(Verdict::Deny, &[("w", Risk::Write)]);
    let (out, s) = fresh(&r, "w", "fresh-w-deny");
    assert!(
        matches!(&out, TurnOutcome::ApprovalRequired { name } if name == "w"),
        "{out:?}"
    );
    assert_eq!(r.consulted(), 1);
    assert_eq!(r.runs(), 0);
    assert_eq!(
        turn_errors(&s),
        vec![("approval required".to_string(), "w".to_string())]
    );
    // No sandwich was opened, session parked in Planning, no wrote fact.
    assert_eq!(count_kind(&s, "intent"), 0);
    assert_eq!(s.state().pc, Pc::Planning);
    assert_eq!(wrote_flag(&s), None);
    // The error is parented under the turn's user message.
    let user = s
        .entries()
        .iter()
        .find(|e| e.kind.as_str() == "message")
        .unwrap();
    let err = s
        .entries()
        .iter()
        .find(|e| e.kind.as_str() == "error")
        .unwrap();
    assert_eq!(err.parent_id.as_deref(), Some(user.id.as_str()));
}

#[test]
fn gate_char_fresh_destructive_goes_through_the_same_approver_gate() {
    // drive() has no Destructive-specific branch today: the structural
    // protection is registration-time (interactive approver required).
    let r = rig(Verdict::Allow, &[("d", Risk::Destructive)]);
    let (out, s) = fresh(&r, "d", "fresh-d-allow");
    assert!(
        matches!(out, TurnOutcome::Final { wrote: true, .. }),
        "{out:?}"
    );
    assert_eq!(r.seen.lock().unwrap()[0], ("d".to_string(), "Destructive"));
    assert_eq!(r.runs(), 1);
    assert_eq!(wrote_flag(&s), Some(json!(true)));

    let r = rig(Verdict::Deny, &[("d", Risk::Destructive)]);
    let (out, s) = fresh(&r, "d", "fresh-d-deny");
    assert!(
        matches!(&out, TurnOutcome::ApprovalRequired { name } if name == "d"),
        "{out:?}"
    );
    assert_eq!(r.runs(), 0);
    assert_eq!(
        turn_errors(&s),
        vec![("approval required".to_string(), "d".to_string())]
    );
}

#[test]
fn gate_char_fresh_unknown_tool() {
    let r = rig(Verdict::Allow, &[("w", Risk::Write)]);
    let (out, s) = fresh(&r, "ghost", "fresh-unknown");
    assert!(
        matches!(&out, TurnOutcome::UnknownTool { name } if name == "ghost"),
        "{out:?}"
    );
    assert_eq!(r.consulted(), 0);
    assert_eq!(
        turn_errors(&s),
        vec![("unknown tool".to_string(), "ghost".to_string())]
    );
    assert_eq!(count_kind(&s, "intent"), 0);
    assert_eq!(s.state().pc, Pc::Final);
}

#[test]
fn gate_char_fresh_cancel_while_permission_pending_settles_cancelled_not_refusal() {
    let cancel = CancelToken::new();
    let r = rig_with(Verdict::Deny, Some(cancel.clone()), &[("w", Risk::Write)]);
    let dir = tmpdir("fresh-cancel-pending");
    let mut s = JsonlStorage::create(&dir, "s", None).unwrap();
    let mut p = MockProvider::scripted(vec![call("w"), final_out("done")]);
    let out = run_turn_with_cancel(&mut s, &mut p, &r.reg, "go", &cancel).unwrap();
    assert!(matches!(out, TurnOutcome::Cancelled), "{out:?}");
    assert_eq!(
        turn_errors(&s),
        vec![(
            "cancelled".to_string(),
            "client cancelled the turn (session/cancel)".to_string()
        )]
    );
    assert_eq!(s.state().pc, Pc::Final);
    assert_eq!(r.runs(), 0);
    assert_eq!(count_kind(&s, "intent"), 0);
}

#[test]
fn gate_char_fresh_cancel_after_allow_stops_before_execution() {
    // Approver allows but the cancel lands while it is pending: the gate
    // passes, then the post-begin cancel checkpoint settles the fresh
    // intent as failed and the turn as cancelled.
    let cancel = CancelToken::new();
    let r = rig_with(Verdict::Allow, Some(cancel.clone()), &[("w", Risk::Write)]);
    let dir = tmpdir("fresh-cancel-allow");
    let mut s = JsonlStorage::create(&dir, "s", None).unwrap();
    let mut p = MockProvider::scripted(vec![call("w"), final_out("done")]);
    let out = run_turn_with_cancel(&mut s, &mut p, &r.reg, "go", &cancel).unwrap();
    assert!(matches!(out, TurnOutcome::Cancelled), "{out:?}");
    assert_eq!(r.runs(), 0);
    assert_eq!(count_kind(&s, "intent"), 1);
    assert_eq!(settle_errors(&s), vec!["cancelled before execution"]);
    assert_eq!(s.state().pc, Pc::Final);
    assert_eq!(wrote_flag(&s), None);
}

#[cfg(feature = "shell-tools")]
#[test]
fn gate_char_fresh_pre_hook_deny_records_pre_hook_denial() {
    let mut r = rig(Verdict::Allow, &[("w", Risk::Write)]);
    let (cmd, marker) = hooks::hook("fresh-hook-deny", 2, "nope");
    hooks::set_pre(&mut r, &cmd);
    let (out, s) = fresh(&r, "w", "fresh-hook-deny-run");
    assert!(
        matches!(&out, TurnOutcome::ApprovalRequired { name } if name == "w"),
        "{out:?}"
    );
    assert!(marker.exists());
    assert_eq!(r.consulted(), 1, "approver is consulted first");
    assert_eq!(r.runs(), 0);
    assert_eq!(
        turn_errors(&s),
        vec![(
            "pre-hook denial".to_string(),
            "w: denied by pretool hook: nope".to_string()
        )]
    );
    assert_eq!(count_kind(&s, "intent"), 0);
    assert_eq!(wrote_flag(&s), None);
}

#[cfg(feature = "shell-tools")]
#[test]
fn gate_char_fresh_pre_hook_pass_and_non_deny_failure_do_not_block() {
    // exit 0 = pass; any non-2 exit is a non-blocking hook failure.
    for (code, tag) in [(0, "fresh-hook-pass"), (1, "fresh-hook-fail")] {
        let mut r = rig(Verdict::Allow, &[("w", Risk::Write)]);
        let (cmd, marker) = hooks::hook(tag, code, "");
        hooks::set_pre(&mut r, &cmd);
        let (out, s) = fresh(&r, "w", tag);
        assert!(
            matches!(out, TurnOutcome::Final { wrote: true, .. }),
            "{out:?}"
        );
        assert!(marker.exists());
        assert_eq!(r.runs(), 1);
        assert!(turn_errors(&s).is_empty());
    }
}

#[cfg(feature = "shell-tools")]
#[test]
fn gate_char_fresh_order_approver_before_pre_hook() {
    // A denied approval must never run the hook.
    let mut r = rig(Verdict::Deny, &[("w", Risk::Write)]);
    let (cmd, marker) = hooks::hook("fresh-order", 2, "nope");
    hooks::set_pre(&mut r, &cmd);
    let (out, s) = fresh(&r, "w", "fresh-order-run");
    assert!(
        matches!(out, TurnOutcome::ApprovalRequired { .. }),
        "{out:?}"
    );
    assert!(!marker.exists(), "hook ran despite denied approval");
    assert_eq!(
        turn_errors(&s),
        vec![("approval required".to_string(), "w".to_string())],
        "the approval refusal (not the hook denial) is what is recorded"
    );
}

#[cfg(feature = "shell-tools")]
#[test]
fn gate_char_fresh_post_hook_runs_only_for_non_readonly() {
    let mut r = rig(Verdict::Allow, &[("w", Risk::Write)]);
    let (cmd, marker) = hooks::hook("fresh-post-w", 0, "");
    hooks::set_post(&mut r, &cmd);
    let (out, _s) = fresh(&r, "w", "fresh-post-w-run");
    assert!(matches!(out, TurnOutcome::Final { .. }), "{out:?}");
    assert!(marker.exists(), "post-hook must fire for a Write tool");
}

// ===========================================================================
// 2. REPLAY path (`resume_turn`)
// ===========================================================================

#[test]
fn gate_char_replay_guarded_deny_settles_err_and_records_approval_required() {
    let r = rig(Verdict::Deny, &[("w", Risk::Write)]);
    let (mut s, id) = seed_pending("rp-g-deny", "w", json!({"n": 1}), ReplaySafety::Guarded);
    let out = replay(&r, &mut s, vec![final_out("replanned")]);
    assert!(
        matches!(&out, TurnOutcome::ApprovalRequired { name } if name == "w"),
        "{out:?}"
    );
    assert_eq!(r.consulted(), 1);
    assert_eq!(r.runs(), 0);
    assert_eq!(
        settle_errors(&s),
        vec!["replay denied: no fresh approval for a guarded effect"]
    );
    assert_eq!(
        turn_errors(&s),
        vec![(
            "approval required".to_string(),
            format!("guarded intent {id} replay denied (no fresh approval)")
        )]
    );
    // Sandwich closed (no livelock), parked in Planning.
    assert!(s.get_register("pending", "op").is_none());
    assert_eq!(s.state().pc, Pc::Planning);
    assert_eq!(wrote_flag(&s), None);
}

#[test]
fn gate_char_replay_guarded_allow_executes_and_sets_wrote() {
    let r = rig(Verdict::Allow, &[("w", Risk::Write)]);
    let (mut s, _id) = seed_pending("rp-g-allow", "w", json!({"n": 1}), ReplaySafety::Guarded);
    let out = replay(&r, &mut s, vec![final_out("after")]);
    match out {
        TurnOutcome::Final { text, wrote } => {
            assert_eq!(text, "after");
            assert!(wrote, "#143: a replayed Write still counts as a write");
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(r.consulted(), 1);
    assert_eq!(r.runs(), 1);
    assert_eq!(wrote_flag(&s), Some(json!(true)));
    assert_eq!(count_kind(&s, "tool_result"), 1);
    assert!(settle_errors(&s).is_empty());
    assert!(turn_errors(&s).is_empty());
}

#[test]
fn gate_char_replay_idempotent_recorded_but_now_write_still_hits_the_gate() {
    // #249: the recorded safety says Idempotent, the tool is Write NOW.
    let r = rig(Verdict::Deny, &[("w", Risk::Write)]);
    let (mut s, id) = seed_pending(
        "rp-249-deny",
        "w",
        json!({"n": 1}),
        ReplaySafety::Idempotent,
    );
    let out = replay(&r, &mut s, vec![final_out("x")]);
    assert!(
        matches!(out, TurnOutcome::ApprovalRequired { .. }),
        "{out:?}"
    );
    assert_eq!(r.consulted(), 1, "approver must be asked");
    assert_eq!(r.runs(), 0);
    assert_eq!(
        settle_errors(&s),
        vec!["replay denied: no fresh approval for a guarded effect"]
    );
    assert_eq!(
        turn_errors(&s),
        vec![(
            "approval required".to_string(),
            format!("guarded intent {id} replay denied (no fresh approval)")
        )]
    );

    // And with an allowing approver the same intent executes (as a write).
    let r = rig(Verdict::Allow, &[("w", Risk::Write)]);
    let (mut s, _) = seed_pending(
        "rp-249-allow",
        "w",
        json!({"n": 1}),
        ReplaySafety::Idempotent,
    );
    let out = replay(&r, &mut s, vec![final_out("x")]);
    assert!(
        matches!(out, TurnOutcome::Final { wrote: true, .. }),
        "{out:?}"
    );
    assert_eq!(r.consulted(), 1);
    assert_eq!(r.runs(), 1);
    assert_eq!(wrote_flag(&s), Some(json!(true)));
}

#[test]
fn gate_char_replay_idempotent_readonly_executes_without_consultation() {
    let r = rig(Verdict::Deny, &[("r", Risk::ReadOnly)]);
    let (mut s, _) = seed_pending("rp-ro", "r", json!({"n": 1}), ReplaySafety::Idempotent);
    let out = replay(&r, &mut s, vec![final_out("x")]);
    assert!(
        matches!(out, TurnOutcome::Final { wrote: false, .. }),
        "{out:?}"
    );
    assert_eq!(r.consulted(), 0);
    assert_eq!(r.runs(), 1);
    assert_eq!(wrote_flag(&s), None);
}

#[test]
fn gate_char_replay_guarded_intent_for_now_readonly_tool_skips_approver() {
    // Guarded recorded, but the tool is ReadOnly now: needs_gate is true,
    // yet the inner `risk != ReadOnly` check skips the approver, and the
    // replay executes without setting the wrote flag.
    let r = rig(Verdict::Deny, &[("r", Risk::ReadOnly)]);
    let (mut s, _) = seed_pending("rp-g-ro", "r", json!({"n": 1}), ReplaySafety::Guarded);
    let out = replay(&r, &mut s, vec![final_out("x")]);
    assert!(
        matches!(out, TurnOutcome::Final { wrote: false, .. }),
        "{out:?}"
    );
    assert_eq!(r.consulted(), 0);
    assert_eq!(r.runs(), 1);
    assert_eq!(wrote_flag(&s), None);
}

#[test]
fn gate_char_replay_guarded_unregistered_tool() {
    let r = rig(Verdict::Allow, &[("w", Risk::Write)]);
    let (mut s, _) = seed_pending(
        "rp-g-ghost",
        "ghost",
        json!({"n": 1}),
        ReplaySafety::Guarded,
    );
    let out = replay(&r, &mut s, vec![final_out("x")]);
    assert!(
        matches!(&out, TurnOutcome::UnknownTool { name } if name == "ghost"),
        "{out:?}"
    );
    assert_eq!(r.consulted(), 0);
    assert_eq!(
        settle_errors(&s),
        vec!["guarded intent references unregistered tool ghost"]
    );
    assert_eq!(
        turn_errors(&s),
        vec![(
            "unknown tool".to_string(),
            "guarded intent references unregistered tool ghost".to_string()
        )]
    );
    assert!(s.get_register("pending", "op").is_none());
    assert_eq!(s.state().pc, Pc::Final);
}

#[test]
fn gate_char_replay_idempotent_unregistered_tool_settles_unknown_on_resume() {
    // Not gated (no current risk, not Guarded): falls through to the
    // execute step, where the vanished tool settles as a plain error and
    // the loop replans (no UnknownTool outcome, no turn error).
    let r = rig(Verdict::Allow, &[]);
    let (mut s, _) = seed_pending(
        "rp-i-ghost",
        "ghost",
        json!({"n": 1}),
        ReplaySafety::Idempotent,
    );
    let out = replay(&r, &mut s, vec![final_out("replanned")]);
    assert!(
        matches!(&out, TurnOutcome::Final { text, .. } if text == "replanned"),
        "{out:?}"
    );
    assert_eq!(settle_errors(&s), vec!["unknown tool on resume: ghost"]);
    assert!(turn_errors(&s).is_empty());
}

#[test]
fn gate_char_replay_malformed_args_settle_without_executing_or_consulting() {
    for (safety, tag) in [
        (ReplaySafety::Guarded, "rp-bad-g"),
        (ReplaySafety::Idempotent, "rp-bad-i"),
    ] {
        let r = rig(Verdict::Allow, &[("w", Risk::Write)]);
        let (mut s, id) = seed_pending(tag, "w", json!("{not json"), safety);
        let out = replay(&r, &mut s, vec![final_out("replanned")]);
        assert!(
            matches!(&out, TurnOutcome::Final { text, .. } if text == "replanned"),
            "{out:?}"
        );
        assert_eq!(r.consulted(), 0);
        assert_eq!(r.runs(), 0);
        assert_eq!(
            settle_errors(&s),
            vec!["intent carried malformed (non-object) arguments"]
        );
        assert_eq!(
            turn_errors(&s),
            vec![(
                "invalid tool arguments".to_string(),
                format!("replayed intent {id} carried malformed arguments")
            )]
        );
        assert_eq!(wrote_flag(&s), None);
    }
}

#[cfg(feature = "shell-tools")]
#[test]
fn gate_char_replay_pre_hook_deny_after_approval() {
    let mut r = rig(Verdict::Allow, &[("w", Risk::Write)]);
    let (cmd, marker) = hooks::hook("rp-hook-deny", 2, "nope");
    hooks::set_pre(&mut r, &cmd);
    let (mut s, _) = seed_pending(
        "rp-hook-deny-s",
        "w",
        json!({"n": 1}),
        ReplaySafety::Guarded,
    );
    let out = replay(&r, &mut s, vec![final_out("x")]);
    assert!(
        matches!(&out, TurnOutcome::ApprovalRequired { name } if name == "w"),
        "{out:?}"
    );
    assert!(marker.exists());
    assert_eq!(r.consulted(), 1);
    assert_eq!(r.runs(), 0);
    assert_eq!(
        settle_errors(&s),
        vec!["replay denied by pre-hook: denied by pretool hook: nope"]
    );
    assert_eq!(
        turn_errors(&s),
        vec![(
            "pre-hook denial".to_string(),
            "w: denied by pretool hook: nope".to_string()
        )]
    );
    assert!(s.get_register("pending", "op").is_none());
    assert_eq!(wrote_flag(&s), None);
}

#[cfg(feature = "shell-tools")]
#[test]
fn gate_char_replay_order_approver_before_pre_hook() {
    let mut r = rig(Verdict::Deny, &[("w", Risk::Write)]);
    let (cmd, marker) = hooks::hook("rp-order", 2, "nope");
    hooks::set_pre(&mut r, &cmd);
    let (mut s, id) = seed_pending("rp-order-s", "w", json!({"n": 1}), ReplaySafety::Guarded);
    let out = replay(&r, &mut s, vec![final_out("x")]);
    assert!(
        matches!(out, TurnOutcome::ApprovalRequired { .. }),
        "{out:?}"
    );
    assert!(!marker.exists(), "hook ran despite denied approval");
    assert_eq!(
        settle_errors(&s),
        vec!["replay denied: no fresh approval for a guarded effect"]
    );
    assert_eq!(
        turn_errors(&s),
        vec![(
            "approval required".to_string(),
            format!("guarded intent {id} replay denied (no fresh approval)")
        )]
    );
}

#[cfg(feature = "shell-tools")]
#[test]
fn gate_char_replay_readonly_never_runs_pre_hook() {
    let mut r = rig(Verdict::Deny, &[("r", Risk::ReadOnly)]);
    let (cmd, marker) = hooks::hook("rp-ro-hook", 2, "nope");
    hooks::set_pre(&mut r, &cmd);
    let (mut s, _) = seed_pending(
        "rp-ro-hook-s",
        "r",
        json!({"n": 1}),
        ReplaySafety::Idempotent,
    );
    let out = replay(&r, &mut s, vec![final_out("x")]);
    assert!(matches!(out, TurnOutcome::Final { .. }), "{out:?}");
    assert!(!marker.exists());
    assert_eq!(r.runs(), 1);
}

// ---------------------------------------------------------------------------
// #316 / #318 regressions
// ---------------------------------------------------------------------------

#[test]
fn gate_char_fresh_unknown_tool_then_next_turn_succeeds() {
    let r = rig(Verdict::Allow, &[("w", Risk::Write)]);
    let dir = tmpdir("fresh-unknown-next");
    let mut s = JsonlStorage::create(&dir, "s", None).unwrap();
    let mut p = MockProvider::scripted(vec![call("ghost"), final_out("ok")]);
    let out = run_turn(&mut s, &mut p, &r.reg, "go").unwrap();
    assert!(matches!(out, TurnOutcome::UnknownTool { .. }), "{out:?}");
    let out = run_turn(&mut s, &mut p, &r.reg, "again").unwrap();
    assert!(
        matches!(&out, TurnOutcome::Final { text, .. } if text == "ok"),
        "{out:?}"
    );
}

#[test]
fn gate_char_replay_guarded_unknown_tool_then_next_turn_succeeds() {
    let r = rig(Verdict::Allow, &[("w", Risk::Write)]);
    let (mut s, _) = seed_pending(
        "rp-g-ghost-next",
        "ghost",
        json!({"n": 1}),
        ReplaySafety::Guarded,
    );
    let out = replay(&r, &mut s, vec![]);
    assert!(matches!(out, TurnOutcome::UnknownTool { .. }), "{out:?}");
    let mut p = MockProvider::scripted(vec![final_out("ok")]);
    let out = run_turn(&mut s, &mut p, &r.reg, "again").unwrap();
    assert!(
        matches!(&out, TurnOutcome::Final { text, .. } if text == "ok"),
        "{out:?}"
    );
}

#[test]
fn gate_char_resume_with_cancelled_token_settles_cancelled() {
    let r = rig(Verdict::Allow, &[("r", Risk::ReadOnly)]);
    let (mut s, _) = seed_pending("rp-cancel", "r", json!({"n": 1}), ReplaySafety::Idempotent);
    let cancel = CancelToken::new();
    cancel.cancel();
    let mut p = MockProvider::scripted(vec![final_out("never")]);
    let out = resume_turn_with_cancel(&mut s, &mut p, &r.reg, &cancel).unwrap();
    assert!(matches!(out, TurnOutcome::Cancelled), "{out:?}");
    assert_eq!(s.state().pc, Pc::Final);
    assert_eq!(
        turn_errors(&s),
        vec![(
            "cancelled".to_string(),
            "client cancelled the turn (session/cancel)".to_string()
        )]
    );
}
