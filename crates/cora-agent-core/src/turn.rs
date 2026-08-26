//! The single-threaded turn loop (E3): Idle → Planning → (ToolCall →
//! Executing → Settling)* → Final, driven by the durable state machine.
//!
//! Every step is a storage commit or an effect-sandwich call; the loop
//! never mutates in-memory state that is not first persisted. A crash at
//! any await point leaves a replayable log whose resume path equals the
//! no-crash run (proven by the E2 determinism tests at sandwich level and
//! the E5 golden-file test at loop level).

use crate::entry::{EntryType, NewEntry};
use serde_json::json;

use crate::machine::{begin, finish, settle_err, settle_ok, ReplaySafety};
use crate::provider::{Provider, ProviderError, ProviderOutput};
use crate::state::{Pc, StateTransition};
use crate::storage::{Commit, Storage, StorageError};
use crate::tool::{Risk, ToolRegistry};

/// Hard ceiling on provider steps in one turn (loop-guard).
pub const MAX_STEPS: usize = 32;

/// Why a turn ended.
#[derive(Debug)]
pub enum TurnOutcome {
    /// The model returned a final answer.
    Final { text: String },
    /// Step budget exhausted — persistent loop-guard trip.
    BudgetExhausted,
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
/// `pc` must be `Idle` (fresh turn) — resume paths call the machine
/// helpers directly.
pub fn run_turn(
    s: &mut dyn Storage,
    p: &mut dyn Provider,
    registry: &ToolRegistry,
    user_input: &str,
) -> Result<TurnOutcome, StorageError> {
    // Precondition enforced, not just documented: a session already mid-turn
    // (e.g. after ProviderFailed) must be resolved via resume/finish first —
    // driving two turns concurrently is a host bug.
    let current = s.state().pc;
    if current != Pc::Idle {
        return Err(StorageError::Invalid(format!(
            "run_turn requires pc Idle, found {current:?} — resolve the session first (resume/finish)"
        )));
    }
    // Idle → Planning, persisting the user message in the same commit.
    let seq = s.state().seq;
    s.commit(
        Commit::new()
            .entry(NewEntry::root(
                EntryType::new(EntryType::MESSAGE),
                json!({ "role": "user", "text": user_input }),
            ))
            .transition(StateTransition::from(seq, Pc::Planning)),
    )?;

    for _ in 0..MAX_STEPS {
        let transcript = s.entries().to_vec();
        let next = match p.complete(&transcript) {
            Ok(o) => o,
            Err(ProviderError(msg)) => {
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
                let Some(t) = registry.get(&tool) else {
                    return Ok(TurnOutcome::UnknownTool { name: tool });
                };
                if t.risk() != Risk::ReadOnly {
                    return Ok(TurnOutcome::ApprovalRequired { name: tool });
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
        }
    }
    // Durable record of the loop-guard trip: without this commit the budget
    // exhaustion is invisible to replay (pc just sits in Planning). An entry
    // append needs no transition — the machine stays parked for the host.
    s.commit(Commit::new().entry(NewEntry::root(
        EntryType::new(EntryType::ERROR),
        json!({ "error": "budget exhausted", "steps": MAX_STEPS }),
    )))?;
    Ok(TurnOutcome::BudgetExhausted)
}
