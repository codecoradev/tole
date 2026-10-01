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
use crate::register::RegisterWrite;
use crate::state::{Pc, StateTransition};
use crate::storage::{Commit, Storage, StorageError, UsageRecord};
use crate::tool::{Risk, ToolRegistry};

/// Hard ceiling on provider steps in one turn (loop-guard).
pub const MAX_STEPS: usize = 32;

/// Consecutive identical tool calls (same tool + same input) before the
/// loop guard trips (E10). Identical calls are never progress — they are
/// the cheapest failure mode to detect deterministically.
pub const LOOP_TRIP_AFTER: usize = 3;

/// Poll-tool ceiling (#85): polling tools (`job_poll`) legitimately call
/// with identical input for the whole duration of a detached job — the
/// guard's stuck-model heuristic does not apply to them until the model
/// keeps polling far past any sane job-wait budget. 120 polls ≈ hours
/// of attached waiting; a real loop still trips well before token ruin.
pub const POLL_LOOP_TRIP_AFTER: usize = 120;

/// Poll-style tools: identical consecutive input is the CORRECT pattern
/// (the arguments name the job; the result carries the change). Registry
/// classification, not a hardcoded name list, keeps this honest — a new
/// poll tool opts in via `Tool::is_poll()`.
fn is_poll_tool(name: &str) -> bool {
    name == "job_poll"
}

/// One automatic retry for a timeout-classified provider failure per
/// turn (issue #58): missions have died to a transient gateway timeout
/// before any tool ran, while the same session completed end-to-end on a
/// manual resume. The retry is a pure re-read — a failed provider step
/// commits nothing, so the transcript is unchanged and the durable log
/// records the retry decision before it happens.
pub const PROVIDER_TIMEOUT_RETRIES: usize = 1;

/// Why a turn ended.
#[derive(Debug)]
pub enum TurnOutcome {
    /// The model returned a final answer.
    Final { text: String, wrote: bool },
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
    /// A turn-end stop gate (issue #145) denied the final message more
    /// times than the per-turn cap allows; the gate reason is durable.
    StopGateBlocked { reason: String },
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
    // `fact.wrote_this_turn` is deliberately NOT reset here: it is
    // session-scoped (#143) — a session that ever executed a Write is a
    // decision session, including after an abort (post-#84 pc=Final) and
    // across prompt-resumes into fresh run_turn calls.
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
            let intent_id = intent_id.clone();
            // scan-3: an InvalidToolArgs intent records its input as a
            // bare JSON STRING (the raw malformed arguments). Such an
            // intent must NEVER execute — re-settle it as an error and let
            // the provider replan (CodeCora scan-3 finding: the old path
            // replayed it blind, executing the tool with a raw string).
            if !input.is_object() {
                settle_err(
                    s,
                    &EffectHandle {
                        intent_id: intent_id.clone(),
                    },
                    "intent carried malformed (non-object) arguments",
                )?;
                append_turn_error(
                    s,
                    "invalid tool arguments",
                    &format!("replayed intent {intent_id} carried malformed arguments"),
                )?;
            } else {
                if safety == ReplaySafety::Guarded {
                    let Some(t) = registry.get(&tool) else {
                        // Unregistered tool on a Guarded intent: settle the
                        // sandwich as failed so the session stays resumable
                        // (scan #34: never park with an open intent).
                        settle_err(
                            s,
                            &EffectHandle { intent_id },
                            &format!("guarded intent references unregistered tool {tool}"),
                        )?;
                        append_turn_error(
                            s,
                            "unknown tool",
                            &format!("guarded intent references unregistered tool {tool}"),
                        )?;
                        return Ok(TurnOutcome::UnknownTool { name: tool });
                    };
                    if t.risk() != Risk::ReadOnly {
                        // Fresh consent for a replayed non-ReadOnly effect:
                        // actually ASK the wired approver. Allow → proceed to
                        // execute below; deny/absent → settle the sandwich as
                        // failed (loop replans on the error) instead of
                        // returning with the intent permanently pending.
                        match registry.decide(&tool, &input) {
                            Some(Verdict::Allow) => {}
                            _ => {
                                settle_err(
                                    s,
                                    &EffectHandle {
                                        intent_id: intent_id.clone(),
                                    },
                                    "replay denied: no fresh approval for a guarded effect",
                                )?;
                                append_turn_error(
                                    s,
                                    "approval required",
                                    &format!(
                                    "guarded intent {intent_id} replay denied (no fresh approval)"
                                ),
                                )?;
                                return Ok(TurnOutcome::ApprovalRequired { name: tool });
                            }
                        }
                        // scan-3: opt-in pre-hooks apply on replay too — a
                        // hook-deny must not be bypassable by crashing before
                        // settlement (mirrors the normal-path check).
                        #[cfg(feature = "shell-tools")]
                        if let Some(reason) = registry.pre_hook_denial(&tool, &input) {
                            settle_err(
                                s,
                                &EffectHandle {
                                    intent_id: intent_id.clone(),
                                },
                                &format!("replay denied by pre-hook: {reason}"),
                            )?;
                            append_turn_error(s, "pre-hook denial", &format!("{tool}: {reason}"))?;
                            return Ok(TurnOutcome::ApprovalRequired { name: tool });
                        }
                    }
                }
                let handle = EffectHandle { intent_id };
                let out = match registry.get(&tool) {
                    Some(t) => {
                        let risk = t.risk();
                        (t.execute(input), risk)
                    }
                    // The tool vanished between runs (host wiring changed).
                    // The intent is durable — settle it as failed rather than
                    // aborting: the loop replans on the tool_result error.
                    None => (
                        Err(format!("unknown tool on resume: {tool}")),
                        Risk::ReadOnly,
                    ),
                };
                match out {
                    (Ok(o), risk) => {
                        // #143 (cora): replayed Writes are writes too — the
                        // session-scoped flag must see them, or a crash
                        // before first execution misclassifies the session.
                        if risk != Risk::ReadOnly {
                            s.commit(Commit::new().register(RegisterWrite::set(
                                "fact",
                                "wrote_this_turn",
                                json!(true),
                            )))?;
                        }
                        settle_ok(s, &handle, o)?;
                        finish(s)?;
                    }
                    (Err(e), _) => {
                        settle_err(s, &handle, &e)?;
                    }
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
/// Per-turn cap on stop-gate denials (issue #145): after this many
/// gate-forced continuations, the turn settles as `StopGateBlocked`.
pub const STOP_GATE_DENIAL_CAP: u32 = 3;

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
    // Issue #145: turn-end stop-gate denials this turn (bounded — a
    // gate that always denies must not livelock the loop; the E10
    // guard-interaction rule wants the cap testable in isolation).
    #[cfg(feature = "shell-tools")]
    let mut stop_gate_denials: u32 = 0;
    // Issue #143: has this SESSION executed a Write/Destructive tool?
    // Surfaced on Final so the host memory loop can type the session.
    // The flag lives in a durable, session-scoped fact register: set
    // wherever such a tool settles (fresh execution AND crash-replay),
    // never reset by turn machinery — a session that ever wrote is a
    // decision session (cora findings on the first cut: the in-RAM flag
    // died at resume boundaries and replayed writes were invisible).
    let mut wrote = s
        .get_register("fact", "wrote_this_turn")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    // Issue #58: one automatic retry for a timeout-classified provider
    // failure. Budgeted per turn, not per step — a flapping gateway must
    // not get a retry for every step of the same turn.
    let mut timeout_retries_left = PROVIDER_TIMEOUT_RETRIES;
    for _ in 0..MAX_STEPS {
        // No clone: complete() borrows the storage slice. (The old
        // to_vec() allocated the whole transcript every provider step.)
        let next = match p.complete(s.entries()) {
            Ok(o) => o,
            Err(ProviderError(msg)) => {
                // Retry classification (#58 + live 429 evidence): gateway
                // timeouts AND rate-limit rejections are both transient.
                const TRANSIENT: [&str; 2] = ["timeout", "429"];
                if TRANSIENT.iter().any(|k| msg.contains(k)) && timeout_retries_left > 0 {
                    timeout_retries_left -= 1;
                    // Durable, replay-visible decision record BEFORE the
                    // retry fires (same contract as append_turn_error).
                    append_turn_error(
                        s,
                        "provider transient failure, retrying",
                        &format!(
                            "gateway timeout or 429 rate limit; {timeout_retries_left} automatic retry left this turn"
                        ),
                    )?;
                    continue;
                }
                // Durable record, same contract as BudgetExhausted: the
                // failure must be visible to replay, not just the caller.
                append_turn_error(s, "provider failed", &msg)?;
                // #84: settle the turn instead of leaving pc=Planning.
                // A terminal provider failure (non-transient, retries
                // exhausted) previously wedged the session — resume with
                // a PROMPT was refused ("run_turn requires Idle/Final")
                // and the approvals-only recovery is interactive. The
                // state machine already allows Planning→Final; landing
                // here with a durable error record keeps the session
                // plain-resumable: `tole resume <id> "continue"` just
                // works, matching the E5 crash-resume guarantee.
                let seq = s.state().seq;
                s.commit(Commit::new().transition(StateTransition::from(seq, Pc::Final)))?;
                return Ok(TurnOutcome::ProviderFailed { message: msg });
            }
        };
        // Provider-reported usage for THIS step (None for mocks/scripts).
        // Single capture point per step: anchored to the last committed
        // entry — exactly what the request covered as input (the answer
        // itself is not part of its own request). Usage-only commits
        // carry no transition, so they are legal mid-Planning and
        // replay-safe (Record::Usage).
        if let Some(u) = p.last_usage() {
            if let Some(last) = s.entries().last() {
                s.commit(Commit::new().usage(UsageRecord {
                    id: String::new(),
                    entry_id: last.id.clone(),
                    usage: u,
                    cost_usd: None,
                }))?;
            }
        }
        match next {
            ProviderOutput::Final { text } => {
                // Issue #145: turn-end stop gates fire BEFORE the final
                // message commits. Deny (exit 2) = gate-forces-continuation:
                // the Final is NOT committed; the reason lands as a user-
                // role entry the model sees next step, and the loop re-runs.
                // Bounded by STOP_GATE_DENIAL_CAP denials per turn — cap
                // trip settles durably as StopGateBlocked. Post-#84 the
                // abort settles pc=Final (terminal, prompt-resumable).
                #[cfg(feature = "shell-tools")]
                if registry.has_turnend_hooks() {
                    // Per-TURN summary (cora CI): only tool calls after
                    // the last user message count — earlier turns' tools
                    // would re-trigger history-keyed gates on every
                    // later Final of a resumed/chat session.
                    let turn_start = s
                        .entries()
                        .iter()
                        .rposition(|e| {
                            // cora CI round 2: this loop's OWN deny
                            // feedback is also a user-role entry — if it
                            // counted, turn_start would jump past the
                            // turn's tool calls on the second Final and
                            // a presence-keyed gate would be defeated.
                            // Hence the explicit marker is skipped.
                            e.kind.as_str() == "message"
                                && e.payload["role"] == json!("user")
                                && e.payload["stop_gate_feedback"] != json!(true)
                        })
                        .unwrap_or(0);
                    let tools_seen: Vec<(String, Risk)> = s
                        .entries()
                        .iter()
                        .skip(turn_start)
                        // Intents are stored as generic entries whose
                        // payload carries `tool` (the kind is "entry",
                        // not "tool_call" — found by the round-2 test's
                        // payload dump, not by reading code).
                        .filter(|e| e.payload.get("tool").is_some())
                        .filter_map(|e| {
                            let tool = e.payload.get("tool")?.as_str()?.to_string();
                            let risk = registry
                                .get(&tool)
                                .map(|t| t.risk())
                                .unwrap_or(Risk::ReadOnly);
                            Some((tool, risk))
                        })
                        .collect();
                    if let Some(reason) = registry.turnend_denial(&text, &tools_seen) {
                        stop_gate_denials += 1;
                        if stop_gate_denials > STOP_GATE_DENIAL_CAP {
                            append_turn_error(
                                s,
                                "stop gate blocked",
                                &format!(
                                    "denied {stop_gate_denials} times this turn (cap {STOP_GATE_DENIAL_CAP}); last reason: {reason}"
                                ),
                            )?;
                            let seq = s.state().seq;
                            s.commit(
                                Commit::new().transition(StateTransition::from(seq, Pc::Final)),
                            )?;
                            return Ok(TurnOutcome::StopGateBlocked { reason });
                        }
                        s.commit(Commit::new().entry(NewEntry::root(
                            EntryType::new(EntryType::MESSAGE),
                            json!({
                                "role": "user",
                                "text": format!("stop gate: {reason}"),
                                "stop_gate_feedback": true
                            }),
                        )))?;
                        continue;
                    }
                }
                let seq = s.state().seq;
                s.commit(
                    Commit::new()
                        .entry(NewEntry::root(
                            EntryType::new(EntryType::MESSAGE),
                            json!({ "role": "assistant", "text": text.clone() }),
                        ))
                        .transition(StateTransition::from(seq, Pc::Final)),
                )?;
                return Ok(TurnOutcome::Final { text, wrote });
            }
            ProviderOutput::ToolCall { tool, input } => {
                // E10: fingerprint before anything else — the guard must
                // see every call, including ones the registry will refuse.
                let fp = call_fingerprint(&tool, &input);
                streak = if Some(fp) == last_fp { streak + 1 } else { 1 };
                last_fp = Some(fp);
                // Poll-style exemption (#85): for polling tools identical
                // CONSECUTIVE INPUT is the correct calling pattern — the
                // arguments name the same job; the expected change is in
                // the RESULT (running → progress → done). A 14-minute
                // render legitimately polls the same id dozens of times;
                // tripping the guard there aborted healthy missions. The
                // guard still applies once the results stop changing AND
                // the model keeps polling past the patience budget — a
                // much higher ceiling for polls only.
                let trip_at = if is_poll_tool(&tool) {
                    POLL_LOOP_TRIP_AFTER
                } else {
                    LOOP_TRIP_AFTER
                };
                if streak >= trip_at {
                    append_turn_error(
                        s,
                        "loop detected",
                        &format!(
                            "tool {tool} called with identical input {streak} times in a row (guard trips at {trip_at})"
                        ),
                    )?;
                    // #84 consistency: a loop trip is terminal for this
                    // turn — settle to Final so a prompt-resume works.
                    let seq = s.state().seq;
                    s.commit(Commit::new().transition(StateTransition::from(seq, Pc::Final)))?;
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
                    // Opt-in pre-hooks (issue #110, shell-tools only): a
                    // deny (exit 2) settles the same way as an approval
                    // denial — durable turn error + ApprovalRequired.
                    #[cfg(feature = "shell-tools")]
                    if let Some(reason) = registry.pre_hook_denial(&tool, &input) {
                        append_turn_error(s, "pre-hook denial", &format!("{tool}: {reason}"))?;
                        return Ok(TurnOutcome::ApprovalRequired { name: tool });
                    }
                }
                // Planning → ToolCall, then the sandwich. The replay
                // contract derives from RISK, not a blanket Idempotent
                // (CodeCora scan #33): a crash after a Write/Destructive
                // effect ran but before settlement must NOT blindly
                // re-execute on resume — Guarded forces re-consultation
                // of the approver (below) before any replay.
                let safety = if t.risk() == Risk::ReadOnly {
                    ReplaySafety::Idempotent
                } else {
                    ReplaySafety::Guarded
                };
                let seq = s.state().seq;
                s.commit(Commit::new().transition(StateTransition::from(seq, Pc::ToolCall)))?;
                let handle = begin(s, &tool, input.clone(), safety, None)?;
                // Post-hook input snapshot (issue #110): execute consumes
                // `input` by value; hooks observe the exact call input.
                // Write/Destructive ONLY — ReadOnly stays zero-overhead
                // (the documented hook contract; cora-caught).
                #[cfg(feature = "shell-tools")]
                let hook_input = if t.risk() != Risk::ReadOnly && registry.has_post_hooks() {
                    Some(input.clone())
                } else {
                    None
                };
                let out = match t.execute(input) {
                    Ok(o) => o,
                    Err(e) => {
                        // settle_err lands in Planning directly (§10) —
                        // no finish() hop on the failure path.
                        #[cfg(feature = "shell-tools")]
                        if let Some(i) = &hook_input {
                            registry.post_hook_notify(&tool, i, false);
                        }
                        settle_err(s, &handle, &e)?;
                        continue;
                    }
                };
                #[cfg(feature = "shell-tools")]
                if let Some(i) = &hook_input {
                    registry.post_hook_notify(&tool, i, true);
                }
                if t.risk() != Risk::ReadOnly {
                    wrote = true;
                    s.commit(Commit::new().register(RegisterWrite::set(
                        "fact",
                        "wrote_this_turn",
                        json!(true),
                    )))?;
                }
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
                // Replay safety derives from TOOL RISK, not from the fact
                // that nothing executed now (cora scan-3 #50): a crash
                // between this intent and its settlement must re-consult
                // the approval gate for a Write/Destructive tool on
                // resume, exactly like the normal ToolCall path. The
                // intent's input is the RAW malformed arguments, so a
                // guarded replay re-settles it as an error — never an
                // execution.
                let risky = registry
                    .get(&tool)
                    .map(|t| t.risk() != Risk::ReadOnly)
                    .unwrap_or(false);
                let handle = begin(
                    s,
                    &tool,
                    serde_json::Value::String(raw),
                    if risky {
                        ReplaySafety::Guarded
                    } else {
                        ReplaySafety::Idempotent
                    },
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
    // #84 consistency: budget exhaustion is terminal for THIS turn —
    // settle to Final so `resume <id> "prompt"` works next (leaving
    // pc=Planning wedges headless flows exactly like provider failures).
    let seq = s.state().seq;
    s.commit(Commit::new().transition(StateTransition::from(seq, Pc::Final)))?;
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
