//! `TurnCode` is the webhook projection of `ProblemKind` (#1023): its words are
//! a published contract and stay byte for byte what they were.

use tole_cli::webhook_out::turn::{TurnCode, TurnEnd};
use tole_core::problem::ProblemKind;
use tole_core::storage::StorageError;
use tole_core::turn::TurnOutcome as O;

const ALL: [(TurnCode, &str); 7] = [
    (TurnCode::Provider, "provider"),
    (TurnCode::Storage, "storage"),
    (TurnCode::Loop, "loop"),
    (TurnCode::UnknownTool, "unknown_tool"),
    (TurnCode::StopGate, "stop_gate"),
    (TurnCode::Internal, "internal"),
    (TurnCode::Approval, "approval"),
];

#[test]
fn the_code_words_are_the_published_ones_and_the_kinds_words() {
    for (code, word) in ALL {
        assert_eq!(code.as_str(), word);
        assert_eq!(code.as_str(), code.kind().as_str());
        assert_eq!(TurnCode::from_kind(code.kind()), Some(code));
    }
}

#[test]
fn a_budget_stop_has_no_code() {
    assert_eq!(TurnCode::from_kind(ProblemKind::BudgetExhausted), None);
}

#[test]
fn a_failed_or_blocked_end_carries_the_code_of_its_problem_kind() {
    let outcomes = [
        (
            O::ProviderFailed {
                message: "m".into(),
            },
            "provider",
        ),
        (O::Storage(StorageError::Corrupt("c".into())), "storage"),
        (
            O::LoopDetected {
                tool: "t".into(),
                count: 3,
            },
            "loop",
        ),
        (O::UnknownTool { name: "n".into() }, "unknown_tool"),
        (O::StopGateBlocked { reason: "r".into() }, "stop_gate"),
    ];
    for (o, word) in outcomes {
        let kind = o.problem().expect("a problem").kind;
        let end = TurnEnd::from_outcome(&o, 1).expect("announced");
        assert_eq!(end.code.map(TurnCode::as_str), Some(word), "{o:?}");
        assert_eq!(end.code, TurnCode::from_kind(kind), "{o:?}");
    }
}

#[test]
fn an_end_that_is_not_failed_or_blocked_has_no_code_even_with_a_problem() {
    // Approval and budget stops have a problem kind but their end is a status
    // of its own (`approval_pending`, `budget_exhausted`), never a code.
    for o in [O::ApprovalRequired { name: "w".into() }, O::BudgetExhausted] {
        assert_eq!(TurnEnd::from_outcome(&o, 1).unwrap().code, None, "{o:?}");
    }
}

/// The kinds that have no code of their own, on purpose.
const NO_CODE: [ProblemKind; 1] = [ProblemKind::BudgetExhausted];

#[test]
fn every_problem_kind_is_mapped_explicitly() {
    // A kind tole-core adds falls to `internal` in `from_kind`; this fails
    // until the new kind is placed there (or listed in `NO_CODE`).
    for kind in ProblemKind::ALL {
        match TurnCode::from_kind(kind) {
            Some(code) => assert_eq!(code.kind(), kind, "{kind:?} is not placed"),
            None => assert!(NO_CODE.contains(&kind), "{kind:?} has no code"),
        }
    }
}
