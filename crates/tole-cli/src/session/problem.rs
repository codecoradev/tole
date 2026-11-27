//! The words of a settled turn's [`Problem`] (#1023): the one place the CLI
//! host and the TUI turn a kind, a subject and a detail into a sentence.
//! tole-core ships the typed problem only; the wording is a face's job.

use tole_core::problem::{Problem, ProblemKind};
use tole_core::turn::TurnOutcome;

/// Where a sentence is shown. The same problem reads differently on each.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Face {
    /// The operator's stderr line of a session host (`tole: ...`).
    Log,
    /// One short sentence: the TUI's note and the text of a host error.
    Short,
}

/// What `face` says about a settled turn; `None` when the outcome is not a
/// problem (a final answer, a cancel, a detach) or the face stays silent.
pub fn describe_outcome(outcome: &TurnOutcome, face: Face) -> Option<String> {
    describe_problem(&outcome.problem()?, face)
}

/// The sentence for `problem` on `face`; `None` when that face stays silent
/// about it (the host logs nothing for a budget or a storage stop: the stop
/// reason or the error carries them).
pub fn describe_problem(problem: &Problem, face: Face) -> Option<String> {
    let subject = problem.subject.as_deref().unwrap_or_default();
    let detail = problem.detail.as_deref().unwrap_or_default();
    match face {
        Face::Log => match problem.kind {
            ProblemKind::StopGate => Some(format!("tole: stop gate blocked the turn: {detail}")),
            ProblemKind::Approval => Some(format!("tole: approval denied for '{subject}'")),
            ProblemKind::UnknownTool => Some(format!("tole: unknown tool '{subject}'")),
            ProblemKind::Provider => Some(format!("tole: provider failed: {detail}")),
            ProblemKind::Loop => Some(format!("tole: loop detected on '{subject}'")),
            ProblemKind::BudgetExhausted | ProblemKind::Storage | ProblemKind::Internal => None,
            _ => Some(format!("tole: turn stopped ({})", problem.kind.as_str())),
        },
        Face::Short => Some(match problem.kind {
            ProblemKind::Approval => {
                format!("approval denied for '{subject}'; your next message resumes")
            }
            ProblemKind::Provider => format!(
                "provider failed: {detail}{}",
                super::compaction::overflow_hint(detail)
            ),
            ProblemKind::UnknownTool => format!("unknown tool '{subject}'"),
            ProblemKind::BudgetExhausted => "step budget exhausted".into(),
            ProblemKind::StopGate => format!("stop gate blocked: {detail}"),
            ProblemKind::Loop => format!("loop guard tripped on '{subject}'"),
            ProblemKind::Storage => format!("storage error: {detail}"),
            ProblemKind::Internal => "internal error".into(),
            _ => format!("turn stopped ({})", problem.kind.as_str()),
        }),
    }
}
