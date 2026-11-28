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

use crate::secrets::SecretSet;
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

/// The longest `subject` on the wire, in bytes: a tool name the model chose.
pub const MAX_SUBJECT_BYTES: usize = 256;

/// A settled turn that did not finish with an answer.
///
/// Deliberately not `Serialize`: its `detail` is unredacted. The form that
/// leaves the process is [`WireProblem`] (see [`TurnOutcome::problem_redacted`]).
/// `#[non_exhaustive]`: a field can be added without a breaking change.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
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

/// A [`Problem`] fit to leave the process (the `problem` of the `tole serve`
/// prompt answer and of the ACP reply, #1023): `subject` and `detail` are
/// redacted with the session's secrets, THEN cut, so a cut never leaves part
/// of a secret behind. Built only by [`TurnOutcome::problem_redacted`]; no
/// wording.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[non_exhaustive]
pub struct WireProblem {
    pub kind: ProblemKind,
    pub subject: Option<String>,
    pub retryable: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

impl Problem {
    fn redacted(&self, secrets: &SecretSet) -> WireProblem {
        let cut = |t: &String, max| redact_then_cut(t, secrets, max);
        WireProblem {
            kind: self.kind,
            subject: self.subject.as_ref().map(|t| cut(t, MAX_SUBJECT_BYTES)),
            retryable: self.retryable,
            detail: self.detail.as_ref().map(|t| cut(t, MAX_DETAIL_BYTES)),
        }
    }
}

impl TurnOutcome {
    /// The problem a settled turn ended with, its detail cut to
    /// [`MAX_DETAIL_BYTES`]: `None` for a final answer, a cancel and a detach.
    /// The shape for anything that leaves the process.
    pub fn problem(&self) -> Option<Problem> {
        let mut p = self.problem_full()?;
        p.detail = p.detail.map(|d| bounded(&d, MAX_DETAIL_BYTES));
        Some(p)
    }

    /// The problem as it may leave the process, for the `tole serve` and ACP
    /// `problem` field (#1023): `subject` and `detail` redacted with `secrets`
    /// (#677), THEN cut to [`MAX_SUBJECT_BYTES`] and [`MAX_DETAIL_BYTES`] (from
    /// the whole detail, never from an already cut one).
    pub fn problem_redacted(&self, secrets: &SecretSet) -> Option<WireProblem> {
        Some(self.problem_full()?.redacted(secrets))
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

/// `text` as it may leave the process (#1314): redacted with `secrets`, THEN
/// cut to `max` bytes, so a cut never leaves part of a secret behind. The one
/// function behind a [`WireProblem`] and the error text of a face.
pub fn redact_then_cut(text: &str, secrets: &SecretSet, max: usize) -> String {
    bounded(&secrets.redact(text), max)
}

/// `text` cut to `max` bytes on a character boundary.
fn bounded(text: &str, max: usize) -> String {
    const ELLIPSIS: &str = "\u{2026}";
    if text.len() <= max {
        return text.to_string();
    }
    let mut end = max - ELLIPSIS.len();
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}{ELLIPSIS}", &text[..end])
}
