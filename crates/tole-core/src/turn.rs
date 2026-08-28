//! The single-threaded turn loop (E3): Idle → Planning → (ToolCall →
//! Executing → Settling)* → Final, driven by the durable state machine.
//!
//! Every step is a storage commit or an effect-sandwich call; the loop
//! never mutates in-memory state that is not first persisted. A crash at
//! any await point leaves a replayable log whose resume path equals the
//! no-crash run (proven by the E2 determinism tests at sandwich level and
//! the E5 golden-file test at loop level).

use crate::approval::Verdict;
use crate::entry::{EntryType, NewEntry};
use serde_json::{json, Value};

use crate::machine::{
    begin, finish, resume, settle_err, settle_ok, EffectHandle, ReplaySafety, Resume,
};
use crate::provider::{Provider, ProviderError, ProviderOutput};
use crate::state::{Pc, StateTransition};
use crate::storage::{Commit, Storage, StorageError};
use crate::tool::{Risk, ToolRegistry};

/// Hard ceiling on provider steps in one turn (loop-guard).
pub const MAX_STEPS: usize = 32;

/// Consecutive identical tool calls (same tool + same input) before the
/// loop guard trips (E10). Identical calls are never progress — they are
/// the cheapest failure mode to detect deterministically.
pub const LOOP_TRIP_AFTER: usize = 3;

/// Why a turn ended.
#[derive(Debug)]
pub enum TurnOutcome {
    /// The model returned a final answer.
    Final { text: String },
    /// Step budget exhausted — persistent loop-guard trip.
    BudgetExhausted,
    /// The model repeated the same tool call (same tool + same input)
    /// `LOOP_TRIP_AFTER` times in a row (E10 loop guard).
    LoopDetected { tool: String, count: usize },
    /// The provider asked for a tool that is not registered.
    UnknownTool { name: String },
    /// The provider asked for a non-ReadOnly tool: needs the approval
    /// gate (E6); for now it is a durable replan, not a crash.
    ApprovalRequired { name: String },
    /// Provider failed after all retries; the turn aborts.
    ProviderFailed { message: String },
    /// Storage error: abort the turn, session stays consistent.
    Storage(StorageError),
}

/// Drive one full user turn to completion (single-threaded).
///
/// `pc` must be `Idle` (fresh turn) — resume paths call
/// [`resume_turn`] instead.
pub fn run_turn(
    s: &mut dyn Storage,
    p: &mut dyn Provider,
    registry: &ToolRegistry,
    user_input: &str,
) -> Result<TurnOutcome, StorageError> {
    // Precondition enforced, not just documented: the session must be at a
    // turn boundary — Idle (never started / E1 initial state) or Final
    // (previous turn delivered; B1 chat re-opens it). Anything else means a
    // turn is mid-flight (e.g. after ProviderFailed) and must be resolved
    // via resume/finish first — driving two turns concurrently is a host bug.
    let current = s.state().pc;
    if !matches!(current, Pc::Idle | Pc::Final) {
        return Err(StorageError::Invalid(format!(
            "run_turn requires pc Idle or Final, found {current:?} — resolve the session first (resume/finish)"
        )));
    }
    // (Idle|Final) → Planning, persisting the user message in the same commit.
    let seq = s.state().seq;
    s.commit(
        Commit::new()
            .entry(NewEntry::root(
                EntryType::new(EntryType::MESSAGE),
                json!({ "role": "user", "text": user_input }),
            ))
            .transition(StateTransition::from(seq, Pc::Planning)),
    )?;
    drive(s, p, registry)
}

/// Resume a turn interrupted by a crash (or process exit) mid-flight and
/// drive it to completion (E5).
///
/// The recovery protocol, fully driven by durable state — never guesses:
///
/// 1. A set `pending` cell means the crash landed inside an effect
///    sandwich (intent committed, settlement not). The intent's recorded
///    [`ReplaySafety`] contract decides: re-execute (`Idempotent`/
///    `Guarded`) or settle as failed (`Never`).
/// 2. No pending + pc `Settling` means the crash landed between settlement
///    and `finish`; close the sandwich and replan.
/// 3. No pending + pc `Planning` means the crash landed between provider
///    steps; simply continue the loop.
///
/// The re-executed effect settles under the *same* intent id, so the
/// recovered log is structurally identical to an uninterrupted run —
/// the property the E5 golden-file test proves.
pub fn resume_turn(
    s: &mut dyn Storage,
    p: &mut dyn Provider,
    registry: &ToolRegistry,
) -> Result<TurnOutcome, StorageError> {
    match resume(s)? {
        Resume::Clean => {
            let pc_now = s.state().pc;
            match pc_now {
                Pc::Settling => finish(s)?,
                Pc::Planning => {}
                Pc::ToolCall => {
                    // Crash between `Planning → ToolCall` and `begin()`: the
                    // decision was never made durable, so the only honest
                    // exit is to replan (legal per the §5 table).
                    let seq = s.state().seq;
                    s.commit(Commit::new().transition(StateTransition::from(seq, Pc::Planning)))?;
                }
                Pc::Final => {
                    return Err(StorageError::Invalid(
                        "resume_turn on a finished session: nothing to resume".into(),
                    ))
                }
                other => {
                    return Err(StorageError::Invalid(format!(
                        "resume_turn: pc {other:?} with no pending intent is not resumable \
                         (drive it with the machine helpers first)"
                    )))
                }
            }
        }
        Resume::ReExecute {
            intent_id,
            tool,
            input,
            safety,
        } => {
            // The intent's recorded replay contract decides the guard:
            // `Guarded` effects require a fresh approval before replay —
            // may have landed before the effect ever ran, so replaying
            // blind could double-fire a Write/Destructive tool.
            if safety == ReplaySafety::Guarded {
                let Some(t) = registry.get(&tool) else {
                    append_turn_error(
                        s,
                        "unknown tool",
                        &format!("guarded intent {intent_id} references unregistered tool {tool}"),
                    )?;
                    return Ok(TurnOutcome::UnknownTool { name: tool });
                };
                if t.risk() != Risk::ReadOnly {
                    append_turn_error(
                        s,
                        "approval required",
                        &format!("guarded intent {intent_id} needs a fresh approval to replay"),
                    )?;
                    return Ok(TurnOutcome::ApprovalRequired { name: tool });
                }
            }
            let handle = EffectHandle { intent_id };
            let out = match registry.get(&tool) {
                Some(t) => t.execute(input),
                // The tool vanished between runs (host wiring changed).
                // The intent is durable — settle it as failed rather than
                // aborting: the loop replans on the tool_result error.
                None => Err(format!("unknown tool on resume: {tool}")),
            };
            match out {
                Ok(o) => {
                    settle_ok(s, &handle, o)?;
                    finish(s)?;
                }
                Err(e) => {
                    settle_err(s, &handle, &e)?;
                }
            }
        }
        Resume::Fail { intent_id, reason } => {
            let handle = EffectHandle { intent_id };
            settle_err(s, &handle, &reason)?;
        }
    }
    drive(s, p, registry)
}

/// Fingerprint of a tool call for the E10 loop guard: tool name + canonical
/// (sorted-keys) JSON of the input, hashed with the default hasher. Same
/// call → same fingerprint regardless of key order in the input object.
pub fn call_fingerprint(tool: &str, input: &Value) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    tool.hash(&mut h);
    canonical_json(input).hash(&mut h);
    h.finish()
}

/// Canonical JSON string: object keys sorted recursively, so `{a,b}` and
/// `{b,a}` fingerprint identically.
fn canonical_json(v: &Value) -> String {
    match v {
        Value::Object(map) => {
            let mut keys: Vec<&str> = map.keys().map(String::as_str).collect();
            keys.sort();
            let parts: Vec<String> = keys
                .iter()
                .map(|k| format!("{:?}:{}", k, canonical_json(&map[*k])))
                .collect();
            format!("{{{}}}", parts.join(","))
        }
        Value::Array(items) => {
            let parts: Vec<String> = items.iter().map(canonical_json).collect();
            format!("[{}]", parts.join(","))
        }
        _ => serde_json::to_string(v).unwrap_or_default(),
    }
}

/// The planning loop shared by [`run_turn`] and [`resume_turn`]:
/// provider step → (tool sandwich)* → Final, under the step budget.
fn drive(
    s: &mut dyn Storage,
    p: &mut dyn Provider,
    registry: &ToolRegistry,
) -> Result<TurnOutcome, StorageError> {
    // E10 loop guard: fingerprint of the last tool call (tool + canonical
    // input), with its consecutive repeat count. Identical calls are
    // never progress.
    let mut last_fp: Option<u64> = None;
    let mut streak: usize = 0;
    for _ in 0..MAX_STEPS {
        let transcript = s.entries().to_vec();
        let next = match p.complete(&transcript) {
            Ok(o) => o,
            Err(ProviderError(msg)) => {
                // Durable record, same contract as BudgetExhausted: the
                // failure must be visible to replay, not just the caller.
                append_turn_error(s, "provider failed", &msg)?;
                return Ok(TurnOutcome::ProviderFailed { message: msg });
            }
        };
        match next {
            ProviderOutput::Final { text } => {
                let seq = s.state().seq;
                s.commit(
                    Commit::new()
                        .entry(NewEntry::root(
                            EntryType::new(EntryType::MESSAGE),
                            json!({ "role": "assistant", "text": text.clone() }),
                        ))
                        .transition(StateTransition::from(seq, Pc::Final)),
                )?;
                return Ok(TurnOutcome::Final { text });
            }
            ProviderOutput::ToolCall { tool, input } => {
                // E10: fingerprint before anything else — the guard must
                // see every call, including ones the registry will refuse.
                let fp = call_fingerprint(&tool, &input);
                streak = if Some(fp) == last_fp { streak + 1 } else { 1 };
                last_fp = Some(fp);
                if streak >= LOOP_TRIP_AFTER {
                    append_turn_error(
                        s,
                        "loop detected",
                        &format!(
                            "tool {tool} called with identical input {streak} times in a row (guard trips at {LOOP_TRIP_AFTER})"
                        ),
                    )?;
                    return Ok(TurnOutcome::LoopDetected {
                        tool,
                        count: streak,
                    });
                }
                let Some(t) = registry.get(&tool) else {
                    // Durable record, same contract as the other abort paths.
                    append_turn_error(s, "unknown tool", &tool)?;
                    return Ok(TurnOutcome::UnknownTool { name: tool });
                };
                if t.risk() != Risk::ReadOnly {
                    // Approval gate (E4/E6): the wired approver decides per
                    // call. `Deny` (or no approver reachable here) aborts the
                    // turn with a durable record — never a silent park.
                    match registry.decide(&tool, &input) {
                        Some(Verdict::Allow) => { /* fall through to execute */ }
                        _ => {
                            append_turn_error(s, "approval required", &tool)?;
                            return Ok(TurnOutcome::ApprovalRequired { name: tool });
                        }
                    }
                }
                // Planning → ToolCall, then the sandwich.
                let seq = s.state().seq;
                s.commit(Commit::new().transition(StateTransition::from(seq, Pc::ToolCall)))?;
                let handle = begin(s, &tool, input.clone(), ReplaySafety::Idempotent, None)?;
                let out = match t.execute(input) {
                    Ok(o) => o,
                    Err(e) => {
                        // settle_err lands in Planning directly (§10) —
                        // no finish() hop on the failure path.
                        settle_err(s, &handle, &e)?;
                        continue;
                    }
                };
                settle_ok(s, &handle, out)?;
                finish(s)?;
            }
            ProviderOutput::InvalidToolArgs { tool, raw, reason } => {
                // Malformed `arguments` from the model: record the intent
                // (for auditability) and settle it as an error WITHOUT
                // executing anything. The model sees the parse error in the
                // next request and can retry with well-formed JSON.
                let seq = s.state().seq;
                s.commit(Commit::new().transition(StateTransition::from(seq, Pc::ToolCall)))?;
                let handle = begin(
                    s,
                    &tool,
                    serde_json::Value::String(raw),
                    ReplaySafety::Idempotent,
                    None,
                )?;
                let msg = format!("tool arguments are not valid JSON: {reason}");
                append_turn_error(s, "invalid tool arguments", &msg)?;
                settle_err(s, &handle, &msg)?;
                continue;
            }
        }
    }
    append_turn_error(s, "budget exhausted", &format!("{MAX_STEPS} steps"))?;
    Ok(TurnOutcome::BudgetExhausted)
}

/// Appends a durable ERROR entry under the turn's user message. No
/// transition: the machine stays parked in Planning for the host to
/// resolve (resume/finish) — the record exists so replay can see why.
fn append_turn_error(s: &mut dyn Storage, error: &str, detail: &str) -> Result<(), StorageError> {
    let parent = s
        .entries()
        .iter()
        .rev()
        .find(|e| e.kind.as_str() == "message" && e.payload["role"] == json!("user"))
        .map(|e| e.id.clone());
    s.commit(Commit::new().entry(NewEntry {
        id: None,
        parent_id: parent,
        kind: EntryType::new(EntryType::ERROR),
        payload: json!({ "error": error, "detail": detail }),
        timestamp: 0,
    }))?;
    Ok(())
}
