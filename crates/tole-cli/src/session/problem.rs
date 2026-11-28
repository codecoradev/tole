//! The words of a settled turn's [`Problem`] (#1023): the one place the CLI
//! host and the TUI turn a kind, a subject and a detail into a sentence.
//! tole-core ships the typed problem only; the wording is a face's job.

use tole_core::problem::{redact_then_cut, Problem, ProblemKind, WireProblem, MAX_DETAIL_BYTES};
use tole_core::secrets::SecretSet;
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
/// problem (a final answer, a cancel, a detach) or the face stays silent. It
/// reads the whole detail (`problem_full`), not the cut one meant for a sink
/// that leaves the process, so the printed text never shrinks.
pub fn describe_outcome(outcome: &TurnOutcome, face: Face) -> Option<String> {
    describe_problem(&outcome.problem_full()?, face)
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

/// The problem of a settled turn as it leaves the process (#1023): `subject`
/// and `detail` redacted with `secrets` (the session storage's
/// `egress_secrets()`), then cut. `None` for a final answer, a cancel and a
/// detach.
pub fn wire(outcome: &TurnOutcome, secrets: &SecretSet) -> Option<WireProblem> {
    outcome.problem_redacted(secrets)
}

/// An error text of a face as it leaves the process (#1314): the serve `error`
/// of a 500 and the ACP error reply, redacted with the process egress set then
/// cut, like the `detail` of [`wire`].
pub fn egress_error(text: &str) -> String {
    redact_then_cut(text, &crate::approvals::egress_secrets(), MAX_DETAIL_BYTES)
}

/// The `tole serve` prompt answer (`POST /sessions/{id}/prompt`): the queued
/// approval's body when the turn waits for one, else `stopReason` and the text.
/// `problem` is added next to those fields; nothing else changes.
pub fn prompt_answer(
    approval: Option<serde_json::Value>,
    stop: &str,
    text: Option<String>,
    problem: Option<&WireProblem>,
) -> serde_json::Value {
    let mut body = match (approval, text) {
        (Some(body), _) => body,
        (None, Some(t)) => serde_json::json!({"stopReason": stop, "text": t}),
        (None, None) => serde_json::json!({"stopReason": stop}),
    };
    if let Some(p) = problem {
        body["problem"] = serde_json::json!(p);
    }
    body
}

/// The ACP `session/prompt` result. `stopReason` is the fixed ACP enum and
/// stays as is; the problem rides in `_meta.tole.problem`, the extension point
/// the ACP schema reserves on every type.
pub fn prompt_result(stop: &str, problem: Option<&WireProblem>) -> serde_json::Value {
    let mut out = serde_json::json!({ "stopReason": stop });
    if let Some(p) = problem {
        out["_meta"] = serde_json::json!({"tole": {"problem": p}});
    }
    out
}
