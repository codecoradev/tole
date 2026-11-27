//! One mapping from a settled turn to a typed problem (#1023).
//!
//! [`TurnOutcome::problem`] is the single exhaustive `match` from an outcome to
//! a [`Problem`]: a closed kind, the thing it is about, whether a plain retry
//! can help, and the technical text. It carries NO wording: a face (CLI, TUI,
//! a graphical client, a chat gateway) words a problem itself, and tole-core
//! stays free of UI copy. Nothing here is persisted: the problem is derived
//! from the outcome when it is read, so the log format does not change.

use serde::Serialize;

use crate::turn::TurnOutcome;

/// The longest `detail`, in bytes (cut on a character boundary, ending in an
/// ellipsis). Provider error text is already a short snippet; this is the
/// ceiling for the rest.
pub const MAX_DETAIL_BYTES: usize = 4096;

/// What went wrong. The wire name is [`ProblemKind::as_str`] (snake_case).
///
/// `#[non_exhaustive]`: new kinds will come (finer provider causes such as a
/// rate limit or a sign-out), and a match outside this crate must keep a
/// fallback arm so adding one is not a breaking change.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[non_exhaustive]
#[serde(rename_all = "snake_case")]
pub enum ProblemKind {
    /// The provider failed after all retries.
    Provider,
    /// The session store failed.
    Storage,
    /// The model repeated the same tool call (the loop guard tripped).
    Loop,
    /// The model asked for a tool that is not registered.
    UnknownTool,
    /// A turn-end stop gate denied the final answer too many times.
    StopGate,
    /// The turn stopped on a tool call that needs an approval it did not get.
    Approval,
    /// A failure of the host itself, not of one of the parts above.
    Internal,
    /// The step budget ran out.
    #[serde(rename = "budget")]
    BudgetExhausted,
}

impl ProblemKind {
    /// The wire name; equal to the serde name.
    pub fn as_str(self) -> &'static str {
        match self {
            ProblemKind::Provider => "provider",
            ProblemKind::Storage => "storage",
            ProblemKind::Loop => "loop",
            ProblemKind::UnknownTool => "unknown_tool",
            ProblemKind::StopGate => "stop_gate",
            ProblemKind::Approval => "approval",
            ProblemKind::Internal => "internal",
            ProblemKind::BudgetExhausted => "budget",
        }
    }
}

/// A settled turn that did not finish with an answer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Problem {
    pub kind: ProblemKind,
    /// What it is about: the tool of a loop, an unknown tool or an approval.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub subject: Option<String>,
    /// A plain retry of the same turn may succeed with nobody changing
    /// anything (a transient provider failure). Everything else needs a
    /// decision, a fix or a different prompt first.
    pub retryable: bool,
    /// The technical text (a provider message, a gate reason, a storage
    /// error), cut to [`MAX_DETAIL_BYTES`]. It is NOT redacted here: a sink
    /// that leaves the process redacts it with the session's secrets (#677).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

impl TurnOutcome {
    /// The problem a settled turn ended with: `None` for a final answer, a
    /// cancel and a detach. The one exhaustive mapping from an outcome: a new
    /// variant does not compile until it is placed here.
    pub fn problem(&self) -> Option<Problem> {
        use TurnOutcome as O;
        let (kind, subject, retryable, detail) = match self {
            O::Final { .. } | O::Cancelled | O::Detached => return None,
            O::BudgetExhausted => (ProblemKind::BudgetExhausted, None, false, None),
            O::LoopDetected { tool, .. } => (ProblemKind::Loop, Some(tool.clone()), false, None),
            O::UnknownTool { name } => (ProblemKind::UnknownTool, Some(name.clone()), false, None),
            O::ApprovalRequired { name } => {
                (ProblemKind::Approval, Some(name.clone()), false, None)
            }
            O::ProviderFailed { message } => {
                (ProblemKind::Provider, None, true, Some(message.clone()))
            }
            O::StopGateBlocked { reason } => {
                (ProblemKind::StopGate, None, false, Some(reason.clone()))
            }
            O::Storage(e) => (ProblemKind::Storage, None, false, Some(e.to_string())),
        };
        Some(Problem {
            kind,
            subject,
            retryable,
            detail: detail.map(|d| bounded(&d)),
        })
    }
}

/// `text` cut to [`MAX_DETAIL_BYTES`] on a character boundary.
fn bounded(text: &str) -> String {
    const ELLIPSIS: &str = "\u{2026}";
    if text.len() <= MAX_DETAIL_BYTES {
        return text.to_string();
    }
    let mut end = MAX_DETAIL_BYTES - ELLIPSIS.len();
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}{ELLIPSIS}", &text[..end])
}
