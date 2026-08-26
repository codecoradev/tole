//! Operation state machine: program counter + CAS token.
//!
//! The whole durable state machine is a singleton: a program counter (`pc`)
//! naming the current step, plus a `seq` token used for compare-and-swap
//! transitions. A transition is only valid when the caller presents the
//! `expected_seq` the machine currently holds; otherwise the write is
//! rejected as stale (double-apply / race detection).
//!
//! Transition legality (which `pc` may follow which) is enforced from Phase
//! 1b (E2); Phase 1a provides the representation and CAS semantics only.

use serde::{Deserialize, Serialize};

/// The steps of the operation loop.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Pc {
    /// Waiting for user input.
    Idle,
    /// Planning the next action.
    Planning,
    /// A tool call is being prepared.
    ToolCall,
    /// A risky tool is waiting for approval.
    AwaitingApproval,
    /// The tool effect is running.
    Executing,
    /// Writing the settlement of a finished effect.
    Settling,
    /// Terminal: final answer delivered.
    Final,
}

/// The durable machine state (singleton register).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MachineState {
    /// Current step of the operation loop.
    pub pc: Pc,
    /// CAS token: the global `seq` observed at the last transition.
    /// The next transition must present exactly this value as `expected_seq`.
    pub seq: u64,
}

impl Default for MachineState {
    fn default() -> Self {
        Self {
            pc: Pc::Idle,
            seq: 0,
        }
    }
}

/// A CAS-guarded state transition inside a [`Commit`](crate::storage::Commit).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StateTransition {
    /// The `state.seq` value the caller observed when deciding the transition.
    pub expected_seq: u64,
    /// The next program-counter step.
    pub new_pc: Pc,
}

impl StateTransition {
    /// Builds a transition from the currently observed `(pc, seq)`.
    pub fn from(expected_seq: u64, new_pc: Pc) -> Self {
        Self {
            expected_seq,
            new_pc,
        }
    }
}

/// Returns `true` when `from -> to` is a legal step of the operation loop
/// (architecture §5 state diagram).
pub fn can_transition(from: Pc, to: Pc) -> bool {
    use Pc::*;
    matches!(
        (from, to),
        (Idle, Planning)
            | (Planning, ToolCall)
            | (Planning, Final)
            | (ToolCall, AwaitingApproval)
            | (ToolCall, Executing)
            | (AwaitingApproval, Executing)
            | (AwaitingApproval, Planning)
            | (Executing, Settling)
            | (Executing, Planning)
            | (Settling, Planning)
    )
}
