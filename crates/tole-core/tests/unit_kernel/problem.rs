//! The one mapping from a settled turn to a typed problem (#1023).

use serde_json::json;
use tole_core::problem::{Problem, ProblemKind, MAX_DETAIL_BYTES, MAX_SUBJECT_BYTES};
use tole_core::secrets::SecretSet;
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
    let words = [
        "provider",
        "storage",
        "loop",
        "unknown_tool",
        "stop_gate",
        "approval",
        "internal",
        "budget",
    ];
    for (kind, word) in ProblemKind::ALL.into_iter().zip(words) {
        assert_eq!(kind.as_str(), word);
        assert_eq!(serde_json::to_value(kind).unwrap(), json!(word));
    }
}

#[test]
fn the_kind_alone_and_the_whole_detail_are_available_without_the_cut() {
    let long = "x".repeat(MAX_DETAIL_BYTES * 3);
    let o = O::ProviderFailed {
        message: long.clone(),
    };
    assert_eq!(o.problem_kind(), Some(ProblemKind::Provider));
    assert_eq!(
        o.problem_full().unwrap().detail.as_deref(),
        Some(long.as_str())
    );
    assert!(o.problem().unwrap().detail.unwrap().len() <= MAX_DETAIL_BYTES);
    assert_eq!(O::Cancelled.problem_kind(), None);
    assert_eq!(O::Detached.problem_full(), None);
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

const KEY: &str = "sk-live-0123456789abcdefXYZ";

fn secrets() -> SecretSet {
    SecretSet::collect(Vec::new(), [("PROVIDER_KEY".to_string(), KEY.to_string())])
}

#[test]
fn the_wire_form_redacts_the_detail_and_the_subject_with_the_session_secrets() {
    let o = O::ProviderFailed {
        message: format!("401 from the provider: bad key {KEY}"),
    };
    let w = o.problem_redacted(&secrets()).expect("problem");
    let detail = w.detail.expect("detail");
    assert!(!detail.contains(KEY), "{detail}");
    assert!(detail.contains("\u{ab}PROVIDER_KEY\u{bb}"), "{detail}");

    let o = O::UnknownTool {
        name: format!("tool_{KEY}"),
    };
    let subject = o
        .problem_redacted(&secrets())
        .and_then(|w| w.subject)
        .expect("subject");
    assert!(!subject.contains(KEY), "{subject}");
}

#[test]
fn a_secret_that_straddles_the_cut_is_redacted_before_it_is_cut() {
    // The secret starts 20 bytes before the cap: cutting first would leave
    // its head behind, which no redaction could match any more.
    let message = format!(
        "{} {KEY} {}",
        "a".repeat(MAX_DETAIL_BYTES - 21),
        "b".repeat(50)
    );
    let o = O::ProviderFailed { message };
    let w = o.problem_redacted(&secrets()).expect("problem");
    let detail = w.detail.expect("detail");
    assert!(!detail.contains(&KEY[..8]), "a head of the secret is left");
    assert!(detail.len() <= MAX_DETAIL_BYTES);
}

#[test]
fn the_wire_form_is_cut_and_has_the_documented_json_shape() {
    let long = O::UnknownTool {
        name: "n".repeat(MAX_SUBJECT_BYTES * 2),
    };
    let w = long.problem_redacted(&secrets()).expect("problem");
    assert_eq!(w.subject.as_deref().map(str::len), Some(MAX_SUBJECT_BYTES));

    let o = O::ProviderFailed {
        message: "overloaded".into(),
    };
    assert_eq!(
        serde_json::to_value(o.problem_redacted(&secrets())).unwrap(),
        json!({"kind": "provider", "subject": null, "retryable": true, "detail": "overloaded"})
    );
    let o = O::ApprovalRequired {
        name: "write".into(),
    };
    assert_eq!(
        serde_json::to_value(o.problem_redacted(&secrets())).unwrap(),
        json!({"kind": "approval", "subject": "write", "retryable": false})
    );
}
