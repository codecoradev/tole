//! The `turn_finished` event's closed types and its provisional policy knobs
//! (#962, design `docs/turn-finished-event.md`).
//!
//! # Provisional owner decisions
//!
//! The design note leaves nine decisions (D1-D9) to the owner. This change
//! implements the note's recommendations as DEFAULTS, and each one is a named
//! constant or one small seam below, so flipping it is a one-line change:
//!
//! | | decision | where |
//! |---|---|---|
//! | D1 | volatile in-memory queue, longer retry horizon | `volatile.rs` queue, [`RETRY_BACKOFF_SECS`], [`RETRY_TAIL_SECS`] |
//! | D2 | the log position is `last_seq`, the `seq` of the newest entry | [`TurnEnd::last_seq`], `event::turn_finished` |
//! | D3 | emit `turn_finished(approval_pending)` | [`EMIT_APPROVAL_PENDING`] |
//! | D4 | `detached` is folded into `cancelled` | [`FOLD_DETACHED_INTO_CANCELLED`] |
//! | D5 | serve sessions only (no mission, no cron) | the call sites in `serve.rs` |
//! | D6 | the status and code words of [`TurnStatus`] and [`TurnCode`] | those enums |
//! | D7 | no `profile` in the body | `event::turn_finished` |
//! | D8 | on overflow replace the oldest event of the session | [`OVERFLOW_REPLACES_OLDEST_PER_SESSION`] |
//! | D9 | a 15 minute retry horizon | [`RETRY_HORIZON_SECS`] |
//! | D10 | the event id carries a per-session settle counter | [`ID_INCLUDES_SETTLE_COUNTER`] |
//!
//! # What may leave
//!
//! A turn end is a typed value with no free text: an enum, an integer and an id.
//! [`TurnEnd::from_outcome`] is the ONLY place a `TurnOutcome` becomes one, it is
//! an exhaustive match with no wildcard arm, and it drops every payload string
//! (a provider message, a tool name, a stop-gate reason, a storage error).

use std::sync::Arc;

use tole_core::problem::ProblemKind;
use tole_core::turn::TurnOutcome;

/// D3: a turn that stops on an approval emits `turn_finished(approval_pending)`.
/// `false` leaves that stop silent, `approval_requested` having already fired.
pub const EMIT_APPROVAL_PENDING: bool = true;

/// D4: an owner detach (`POST /sessions/{id}/detach`) is reported as
/// `cancelled`. `false` reports the separate word `detached`.
pub const FOLD_DETACHED_INTO_CANCELLED: bool = true;

/// D8: when a destination's queue is full, a new `turn_finished` replaces the
/// oldest queued one of the SAME session (only the latest settle matters to a
/// receiver). `false` drops the new event, as the approval events do.
pub const OVERFLOW_REPLACES_OLDEST_PER_SESSION: bool = true;

/// D9: how long a `turn_finished` stays worth sending. A guess; one constant.
pub const RETRY_HORIZON_SECS: i64 = 15 * 60;

/// D1: the waits after the first failed attempts, then [`RETRY_TAIL_SECS`]
/// until [`RETRY_HORIZON_SECS`] passes.
pub const RETRY_BACKOFF_SECS: [i64; 4] = [2, 10, 30, 120];

/// D1: the wait between later attempts.
pub const RETRY_TAIL_SECS: i64 = 300;

/// D10: the event id ends in `:<settle>`, a per-session in-memory counter of
/// settled turns, besides `last_seq`. A turn that fails before it appends
/// anything (a router or provider-config error, a cancel on resume) settles at
/// the SAME `last_seq` as the turn before it, so without the counter its event
/// would repeat the earlier event's id and a receiver deduping on the id would
/// drop it. The counter restarts with the process; across a restart receivers
/// also compare `last_seq`. `false` gives the note's original `...:<last_seq>`.
pub const ID_INCLUDES_SETTLE_COUNTER: bool = true;

/// The serialized `turn_finished` envelope, at most. It is a few hundred bytes
/// by construction; a body over this is refused, never cut.
pub const MAX_TURN_BODY: usize = 1024;

/// How a turn ended (D6). The names are the wire `status` values.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TurnStatus {
    Completed,
    Failed,
    Cancelled,
    /// Only produced when [`FOLD_DETACHED_INTO_CANCELLED`] is `false`.
    Detached,
    BudgetExhausted,
    Blocked,
    ApprovalPending,
}

impl TurnStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            TurnStatus::Completed => "completed",
            TurnStatus::Failed => "failed",
            TurnStatus::Cancelled => "cancelled",
            TurnStatus::Detached => "detached",
            TurnStatus::BudgetExhausted => "budget_exhausted",
            TurnStatus::Blocked => "blocked",
            TurnStatus::ApprovalPending => "approval_pending",
        }
    }
}

/// A fixed reason for `failed` or `blocked` (D6). Never a message. The
/// webhook projection of [`ProblemKind`] (#1023): the words are the kind's.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TurnCode {
    Provider,
    Storage,
    Loop,
    UnknownTool,
    StopGate,
    Internal,
    Approval,
}

impl TurnCode {
    /// The code of a problem kind. `None` for a kind with no code of its own
    /// (`budget` is a status). A kind a newer tole-core adds (the enum is
    /// `non_exhaustive`) reads as `internal` until it is placed here, so an
    /// unknown cause never goes out as a made-up word; the test
    /// `every_problem_kind_is_mapped_explicitly` fails until it is placed.
    pub fn from_kind(kind: ProblemKind) -> Option<TurnCode> {
        match kind {
            ProblemKind::Provider => Some(TurnCode::Provider),
            ProblemKind::Storage => Some(TurnCode::Storage),
            ProblemKind::Loop => Some(TurnCode::Loop),
            ProblemKind::UnknownTool => Some(TurnCode::UnknownTool),
            ProblemKind::StopGate => Some(TurnCode::StopGate),
            ProblemKind::Approval => Some(TurnCode::Approval),
            ProblemKind::Internal => Some(TurnCode::Internal),
            ProblemKind::BudgetExhausted => None,
            _ => Some(TurnCode::Internal),
        }
    }

    /// The problem kind this code is.
    pub fn kind(self) -> ProblemKind {
        match self {
            TurnCode::Provider => ProblemKind::Provider,
            TurnCode::Storage => ProblemKind::Storage,
            TurnCode::Loop => ProblemKind::Loop,
            TurnCode::UnknownTool => ProblemKind::UnknownTool,
            TurnCode::StopGate => ProblemKind::StopGate,
            TurnCode::Internal => ProblemKind::Internal,
            TurnCode::Approval => ProblemKind::Approval,
        }
    }

    /// The wire word: the kind's, byte for byte (a published contract).
    pub fn as_str(self) -> &'static str {
        self.kind().as_str()
    }
}

/// A settled turn, with every payload string already dropped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TurnEnd {
    pub status: TurnStatus,
    pub code: Option<TurnCode>,
    /// D2: the `seq` of the newest log entry when the turn settled, the number
    /// `GET /sessions/{id}/entries?after=` pages by (`lastSeq` / `nextAfter`).
    pub last_seq: u64,
    /// D10: this process's settle count for the session (1 for the first turn
    /// it settled). Part of the event id only; never in the body.
    pub settle: u64,
}

impl TurnEnd {
    /// The same end with its per-session settle number.
    pub fn with_settle(self, settle: u64) -> TurnEnd {
        TurnEnd { settle, ..self }
    }

    /// The one mapping from a `TurnOutcome` to a status. Exhaustive, no
    /// wildcard arm: a new variant does not compile until it is placed here.
    /// `None` is a silent end (D3 off). The code of a `failed` or `blocked`
    /// end is the kind of the outcome (`problem_kind`, #1023); no payload
    /// string is read or copied, and none reaches the end.
    pub fn from_outcome(outcome: &TurnOutcome, last_seq: u64) -> Option<TurnEnd> {
        let status = match outcome {
            TurnOutcome::Final { .. } => TurnStatus::Completed,
            TurnOutcome::ProviderFailed { .. } | TurnOutcome::Storage(_) => TurnStatus::Failed,
            TurnOutcome::Cancelled => TurnStatus::Cancelled,
            TurnOutcome::Detached if FOLD_DETACHED_INTO_CANCELLED => TurnStatus::Cancelled,
            TurnOutcome::Detached => TurnStatus::Detached,
            TurnOutcome::BudgetExhausted => TurnStatus::BudgetExhausted,
            TurnOutcome::LoopDetected { .. }
            | TurnOutcome::UnknownTool { .. }
            | TurnOutcome::StopGateBlocked { .. } => TurnStatus::Blocked,
            TurnOutcome::ApprovalRequired { .. } if EMIT_APPROVAL_PENDING => {
                TurnStatus::ApprovalPending
            }
            TurnOutcome::ApprovalRequired { .. } => return None,
        };
        let code = match status {
            TurnStatus::Failed | TurnStatus::Blocked => {
                outcome.problem().and_then(|p| TurnCode::from_kind(p.kind))
            }
            _ => None,
        };
        Some(TurnEnd {
            status,
            code,
            last_seq,
            settle: 0,
        })
    }

    /// A turn that failed without an outcome: the runner returned an error.
    pub fn failed(code: TurnCode, last_seq: u64) -> TurnEnd {
        TurnEnd {
            status: TurnStatus::Failed,
            code: Some(code),
            last_seq,
            settle: 0,
        }
    }

    /// Settles an approval stop against the queue: `approval_pending` needs the
    /// id of a Pending entry; without one the stop is `blocked` with code
    /// `approval` (a denied or hook-refused call queued nothing to decide).
    /// Any other status passes through, and its `approval_id` is dropped.
    pub fn with_approval(self, approval_id: Option<String>) -> (TurnEnd, Option<String>) {
        if self.status != TurnStatus::ApprovalPending {
            return (self, None);
        }
        match approval_id {
            Some(id) => (self, Some(id)),
            None => (
                TurnEnd {
                    status: TurnStatus::Blocked,
                    code: Some(TurnCode::Approval),
                    ..self
                },
                None,
            ),
        }
    }
}

/// What the serve turn path hands the notifier: ids and a typed end. The
/// approval id comes from the approval queue entry, never from the outcome.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TurnSignal {
    pub session_id: String,
    pub end: TurnEnd,
    /// The Pending queue entry the turn stopped on, if any.
    pub approval_id: Option<String>,
}

/// The callback `tole serve` calls once per settled turn. It does no I/O.
pub type TurnNotifier = Arc<dyn Fn(TurnSignal) + Send + Sync>;
