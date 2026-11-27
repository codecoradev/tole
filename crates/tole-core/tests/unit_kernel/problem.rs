//! The one mapping from a settled turn to a typed problem (#1023).

use serde_json::json;
use tole_core::problem::{Problem, ProblemKind, MAX_DETAIL_BYTES};
use tole_core::storage::StorageError;
use tole_core::turn::TurnOutcome as O;

fn problem(o: &O) -> Problem {
    o.problem().unwrap_or_else(|| panic!("no problem: {o:?}"))
}

#[test]
fn a_final_answer_a_cancel_and_a_detach_are_not_problems() {
    let done = O::Final {
        text: "t".into(),
        wrote: true,
    };
    assert_eq!(done.problem(), None);
    assert_eq!(O::Cancelled.problem(), None);
    assert_eq!(O::Detached.problem(), None);
}

#[test]
fn every_other_outcome_maps_to_its_kind_subject_and_retryability() {
    let s = |x: &str| Some(x.to_string());
    let cases = [
        (
            O::BudgetExhausted,
            ProblemKind::BudgetExhausted,
            None,
            false,
            None,
        ),
        (
            O::LoopDetected {
                tool: "t".into(),
                count: 3,
            },
            ProblemKind::Loop,
            s("t"),
            false,
            None,
        ),
        (
            O::UnknownTool { name: "n".into() },
            ProblemKind::UnknownTool,
            s("n"),
            false,
            None,
        ),
        (
            O::ApprovalRequired { name: "w".into() },
            ProblemKind::Approval,
            s("w"),
            false,
            None,
        ),
        (
            O::ProviderFailed {
                message: "m".into(),
            },
            ProblemKind::Provider,
            None,
            true,
            s("m"),
        ),
        (
            O::StopGateBlocked { reason: "r".into() },
            ProblemKind::StopGate,
            None,
            false,
            s("r"),
        ),
        (
            O::Storage(StorageError::Corrupt("bad".into())),
            ProblemKind::Storage,
            None,
            false,
            s("corrupt session file: bad"),
        ),
    ];
    for (o, kind, subject, retryable, detail) in cases {
        let p = problem(&o);
        assert_eq!(
            (p.kind, p.subject, p.retryable, p.detail),
            (kind, subject, retryable, detail),
            "{o:?}"
        );
    }
}

#[test]
fn the_wire_names_are_snake_case_and_equal_the_serde_names() {
    let kinds = [
        (ProblemKind::Provider, "provider"),
        (ProblemKind::Storage, "storage"),
        (ProblemKind::Loop, "loop"),
        (ProblemKind::UnknownTool, "unknown_tool"),
        (ProblemKind::StopGate, "stop_gate"),
        (ProblemKind::Approval, "approval"),
        (ProblemKind::Internal, "internal"),
        (ProblemKind::BudgetExhausted, "budget"),
    ];
    for (kind, word) in kinds {
        assert_eq!(kind.as_str(), word);
        assert_eq!(serde_json::to_value(kind).unwrap(), json!(word));
    }
}

#[test]
fn a_problem_serializes_without_its_empty_fields() {
    let p = problem(&O::UnknownTool { name: "n".into() });
    assert_eq!(
        serde_json::to_value(&p).unwrap(),
        json!({"kind": "unknown_tool", "subject": "n", "retryable": false})
    );
    let p = problem(&O::ProviderFailed {
        message: "m".into(),
    });
    assert_eq!(
        serde_json::to_value(&p).unwrap(),
        json!({"kind": "provider", "retryable": true, "detail": "m"})
    );
}

#[test]
fn a_detail_up_to_the_cap_is_kept_and_a_longer_one_is_cut_on_a_char_boundary() {
    let at_cap = "a".repeat(MAX_DETAIL_BYTES);
    let kept = problem(&O::ProviderFailed {
        message: at_cap.clone(),
    });
    assert_eq!(kept.detail.as_deref(), Some(at_cap.as_str()));

    // 3-byte characters: a cut must not split one.
    let long = "\u{20ac}".repeat(MAX_DETAIL_BYTES);
    let cut = problem(&O::StopGateBlocked { reason: long })
        .detail
        .expect("detail");
    assert!(cut.len() <= MAX_DETAIL_BYTES, "{}", cut.len());
    assert!(cut.ends_with('\u{2026}'));
    assert!(cut
        .trim_end_matches('\u{2026}')
        .chars()
        .all(|c| c == '\u{20ac}'));
    assert!(cut.len() > MAX_DETAIL_BYTES - 8);
}
