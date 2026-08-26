//! Effect sandwich driver (E2).
//!
//! Every tool effect follows the three-layer sandwich from architecture §6:
//!
//! 1. `begin` — one atomic commit: intent entry + `pending` register cell +
//!    pc → `Executing`.
//! 2. *effect* — the caller runs the real-world side effect with no storage
//!    involvement. This window is exactly where crashes happen.
//! 3. `settle_ok` / `settle_err` — one atomic commit: result/error entry +
//!    clear the `pending` cell + pc advance.
//!
//! ## Crash safety
//!
//! After a crash mid-sandwich, reopening the session finds the `pending`
//! cell set: the intent was durable but the settlement was not. The
//! [`ReplaySafety`] contract recorded in the intent entry decides what
//! [`resume`] reports:
//!
//! - `Idempotent` → the effect may run again (`Resume::ReExecute`).
//! - `Guarded` → same, but the host must verify a tool-specific guard first.
//! - `Never` → the effect must not run again (`Resume::Fail`) — resume
//!   settles it as failed.
//!
//! A crash *after* settlement (pending cleared) is [`Resume::Clean`]; if the
//! pc is still `Settling`, the host finishes with [`finish`].

use crate::entry::{EntryType, NewEntry};
use crate::register::RegisterWrite;
use crate::state::{Pc, StateTransition};
use crate::storage::{Commit, Storage, StorageError};
use serde_json::{json, Value};

/// The single in-flight `pending` cell key (one effect at a time; the turn
/// loop is single-threaded by design).
pub const PENDING_KEY: &str = "op";

/// Per-tool replay contract, persisted inside the intent entry payload.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReplaySafety {
    /// Pure / read-only effect: safe to re-execute after a crash.
    Idempotent,
    /// Re-executable only after the host verifies a tool-specific guard
    /// (e.g. "the row already exists").
    Guarded,
    /// Never re-execute: resume settles the intent as failed.
    Never,
}

impl ReplaySafety {
    /// Stable wire name persisted in the intent payload.
    pub fn as_str(self) -> &'static str {
        match self {
            ReplaySafety::Idempotent => "idempotent",
            ReplaySafety::Guarded => "guarded",
            ReplaySafety::Never => "never",
        }
    }

    /// Parses the wire name recorded in an intent payload.
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "idempotent" => Some(ReplaySafety::Idempotent),
            "guarded" => Some(ReplaySafety::Guarded),
            "never" => Some(ReplaySafety::Never),
            _ => None,
        }
    }
}

/// Handle to one in-flight effect sandwich.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EffectHandle {
    /// Id of the committed intent entry.
    pub intent_id: String,
}

/// Phase 1: persist the intent. One atomic commit line.
///
/// Requires pc `ToolCall` or `AwaitingApproval`; anything else is rejected
/// as an [`StorageError::IllegalTransition`] — the sandwich cannot start
/// from an illegal step.
pub fn begin(
    s: &mut dyn Storage,
    tool: &str,
    input: Value,
    safety: ReplaySafety,
    parent: Option<&str>,
) -> Result<EffectHandle, StorageError> {
    let seq = s.state().seq;
    // Explicit intent id: deterministic, visible to the pending cell in the
    // same atomic commit, and unique (seq strictly advances every commit).
    let intent_id = format!("intent_{seq}");
    let commit = Commit::new()
        .entry(NewEntry {
            id: Some(intent_id.clone()),
            parent_id: parent.map(str::to_string),
            kind: EntryType::new(EntryType::INTENT),
            payload: json!({
                "tool": tool,
                "input": input,
                "replay": safety.as_str(),
            }),
            timestamp: 0,
        })
        .register(RegisterWrite::set(
            "pending",
            PENDING_KEY,
            json!({
                "intentId": intent_id,
                "tool": tool,
                "replay": safety.as_str(),
            }),
        ))
        .transition(StateTransition::from(seq, Pc::Executing));
    s.commit(commit)?;
    Ok(EffectHandle { intent_id })
}

/// Phase 3 (success): persist the settlement. One atomic commit line:
/// tool_result entry (child of the intent) + clear pending + pc → Settling.
///
/// The host then re-enters planning via [`finish`].
pub fn settle_ok(
    s: &mut dyn Storage,
    handle: &EffectHandle,
    output: Value,
) -> Result<String, StorageError> {
    let seq = s.state().seq;
    let commit = Commit::new()
        .entry(NewEntry::with_parent(
            &handle.intent_id,
            EntryType::new(EntryType::TOOL_RESULT),
            json!({ "ok": true, "output": output }),
        ))
        .register(RegisterWrite::delete("pending", PENDING_KEY))
        .transition(StateTransition::from(seq, Pc::Settling));
    let committed = s.commit(commit)?;
    Ok(committed[0].id.clone())
}

/// Phase 3 (failure): settle the intent as failed. One atomic commit line:
/// error entry + clear pending + pc → Planning (§10: an effect error is a
/// settlement, the loop replans — it never panics).
pub fn settle_err(
    s: &mut dyn Storage,
    handle: &EffectHandle,
    message: &str,
) -> Result<String, StorageError> {
    let seq = s.state().seq;
    let commit = Commit::new()
        .entry(NewEntry::with_parent(
            &handle.intent_id,
            EntryType::new(EntryType::ERROR),
            json!({ "ok": false, "error": message }),
        ))
        .register(RegisterWrite::delete("pending", PENDING_KEY))
        .transition(StateTransition::from(seq, Pc::Planning));
    let committed = s.commit(commit)?;
    Ok(committed[0].id.clone())
}

/// Settling → Planning: closes a successful sandwich so the loop can replan.
pub fn finish(s: &mut dyn Storage) -> Result<(), StorageError> {
    let seq = s.state().seq;
    s.commit(Commit::new().transition(StateTransition::from(seq, Pc::Planning)))
        .map(|_| ())
}

/// What [`resume`] found after reopening a session.
#[derive(Debug, Clone, PartialEq)]
pub enum Resume {
    /// No pending intent; the host proceeds normally (calling [`finish`]
    /// first if pc is `Settling`).
    Clean,
    /// A pending intent exists whose replay contract allows another run.
    /// `Guarded` intents must pass a host-side guard before executing.
    ReExecute {
        /// Id of the durable intent entry (use as settlement parent).
        intent_id: String,
        /// Tool name from the intent payload.
        tool: String,
        /// Original tool input.
        input: Value,
        /// Replay contract recorded at `begin` time.
        safety: ReplaySafety,
    },
    /// A pending intent exists whose contract forbids replay: the host must
    /// call [`settle_err`] with `reason`.
    Fail {
        /// Id of the durable intent entry.
        intent_id: String,
        /// Why replay is not allowed.
        reason: String,
    },
}

/// Inspects a reopened session for an unfinished effect sandwich.
///
/// Deterministic: reads only the durable `pending` cell — never guesses.
pub fn resume(s: &mut dyn Storage) -> Result<Resume, StorageError> {
    let Some(pending) = s.get_register("pending", PENDING_KEY).cloned() else {
        return Ok(Resume::Clean);
    };
    let intent_id = pending["intentId"]
        .as_str()
        .ok_or_else(|| StorageError::Corrupt("pending cell is missing intentId".into()))?
        .to_string();
    // The pending cell mirrors the intent payload; the intent entry itself
    // is the authoritative record.
    let entry = s
        .entry(&intent_id)
        .ok_or_else(|| {
            StorageError::Corrupt(format!(
                "pending cell references unknown intent entry {intent_id}"
            ))
        })?
        .clone();
    let payload = &entry.payload;
    let safety =
        ReplaySafety::parse(payload["replay"].as_str().unwrap_or("")).ok_or_else(|| {
            StorageError::Corrupt(format!("intent {intent_id} has unknown replay contract"))
        })?;
    match safety {
        ReplaySafety::Never => Ok(Resume::Fail {
            intent_id,
            reason: "intent carries a never-replay contract".into(),
        }),
        ReplaySafety::Idempotent | ReplaySafety::Guarded => Ok(Resume::ReExecute {
            intent_id,
            tool: payload["tool"].as_str().unwrap_or_default().to_string(),
            input: payload["input"].clone(),
            safety,
        }),
    }
}
