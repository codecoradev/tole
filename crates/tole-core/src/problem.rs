//! One mapping from a settled turn to a typed problem (#1023).
//!
//! [`TurnOutcome::problem`] (and `problem_full`, `problem_kind`) share one
//! exhaustive `match` from an outcome to a [`Problem`]: a closed kind, the thing it is about, whether a plain retry
//! can help, and the technical text. It carries NO wording: a face (CLI, TUI,
//! a graphical client, a chat gateway) words a problem itself, and tole-core
//! stays free of UI copy. Nothing here is persisted: the problem is derived
//! from the outcome when it is read, so the log format does not change.

use std::borrow::Cow;

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
    /// Every kind, for a host that checks its own mapping covers each one.
    /// Keep in step with the enum (the exhaustive `as_str` breaks first).
    pub const ALL: [ProblemKind; 8] = [
        ProblemKind::Provider,
        ProblemKind::Storage,
        ProblemKind::Loop,
        ProblemKind::UnknownTool,
        ProblemKind::StopGate,
        ProblemKind::Approval,
        ProblemKind::Internal,
        ProblemKind::BudgetExhausted,
    ];

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
///
/// Deliberately not `Serialize`: its `detail` is unredacted. The serializable
/// form, with the detail redacted at the egress, arrives with the serve and
/// ACP `problem` field (#1023, part 2).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Problem {
    pub kind: ProblemKind,
    /// What it is about: the tool of a loop, an unknown tool or an approval.
    pub subject: Option<String>,
    /// A plain retry of the same turn may succeed with nobody changing
    /// anything (a transient provider failure). Everything else needs a
    /// decision, a fix or a different prompt first.
    pub retryable: bool,
    /// The technical text (a provider message, a gate reason, a storage
    /// error). From [`TurnOutcome::problem`] it is cut to
    /// [`MAX_DETAIL_BYTES`]; from [`TurnOutcome::problem_full`] it is whole.
    /// It is NOT redacted here: a sink that leaves the process redacts it
    /// with the session's secrets (#677).
    pub detail: Option<String>,
}

impl TurnOutcome {
    /// The problem a settled turn ended with, its detail cut to
    /// [`MAX_DETAIL_BYTES`]: `None` for a final answer, a cancel and a detach.
    /// The shape for anything that leaves the process.
    pub fn problem(&self) -> Option<Problem> {
        let mut p = self.problem_full()?;
        p.detail = p.detail.map(|d| bounded(&d));
        Some(p)
    }

    /// [`TurnOutcome::problem`] with the detail whole, for an in-process face
    /// that already holds the same text in the outcome and must print it
    /// unchanged.
    pub fn problem_full(&self) -> Option<Problem> {
        let (kind, subject, retryable, detail) = self.parts()?;
        Some(Problem {
            kind,
            subject: subject.map(str::to_string),
            retryable,
            detail: detail.map(Cow::into_owned),
        })
    }

    /// Only the kind of [`TurnOutcome::problem`]: no string is copied.
    pub fn problem_kind(&self) -> Option<ProblemKind> {
        Some(self.parts()?.0)
    }

    /// The one exhaustive mapping from an outcome: a new variant does not
    /// compile until it is placed here.
    fn parts(&self) -> Option<Parts<'_>> {
        use TurnOutcome as O;
        Some(match self {
            O::Final { .. } | O::Cancelled | O::Detached => return None,
            O::BudgetExhausted => (ProblemKind::BudgetExhausted, None, false, None),
            O::LoopDetected { tool, .. } => (ProblemKind::Loop, Some(tool.as_str()), false, None),
            O::UnknownTool { name } => (ProblemKind::UnknownTool, Some(name.as_str()), false, None),
            O::ApprovalRequired { name } => {
                (ProblemKind::Approval, Some(name.as_str()), false, None)
            }
            O::ProviderFailed { message } => (
                ProblemKind::Provider,
                None,
                true,
                Some(Cow::Borrowed(message.as_str())),
            ),
            O::StopGateBlocked { reason } => (
                ProblemKind::StopGate,
                None,
                false,
                Some(Cow::Borrowed(reason.as_str())),
            ),
            O::Storage(e) => (
                ProblemKind::Storage,
                None,
                false,
                Some(Cow::Owned(e.to_string())),
            ),
        })
    }
}

/// Kind, subject, retryable and detail of a problem, borrowed from the outcome.
type Parts<'a> = (ProblemKind, Option<&'a str>, bool, Option<Cow<'a, str>>);

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
