//! The words of a settled turn's problem (#1023). Every string below is what
//! the CLI host and the TUI printed before `describe_problem` existed.

use tole_cli::session::problem::{describe_outcome, Face};
use tole_core::storage::StorageError;
use tole_core::turn::TurnOutcome as O;

fn say(o: &O, face: Face) -> Option<String> {
    describe_outcome(o, face)
}

fn all() -> Vec<O> {
    vec![
        O::StopGateBlocked { reason: "r".into() },
        O::ApprovalRequired { name: "n".into() },
        O::UnknownTool { name: "n".into() },
        O::ProviderFailed {
            message: "m".into(),
        },
        O::LoopDetected {
            tool: "t".into(),
            count: 3,
        },
        O::BudgetExhausted,
        O::Storage(StorageError::Corrupt("x".into())),
    ]
}

#[test]
fn the_operator_log_lines_are_unchanged() {
    let want = [
        Some("tole: stop gate blocked the turn: r"),
        Some("tole: approval denied for 'n'"),
        Some("tole: unknown tool 'n'"),
        Some("tole: provider failed: m"),
        Some("tole: loop detected on 't'"),
        None,
        None,
    ];
    for (o, w) in all().iter().zip(want) {
        assert_eq!(say(o, Face::Log).as_deref(), w, "{o:?}");
    }
}

#[test]
fn the_short_sentences_are_unchanged() {
    let want = [
        "stop gate blocked: r",
        "approval denied for 'n'; your next message resumes",
        "unknown tool 'n'",
        "provider failed: m",
        "loop guard tripped on 't'",
        "step budget exhausted",
        "storage error: corrupt session file: x",
    ];
    for (o, w) in all().iter().zip(want) {
        assert_eq!(say(o, Face::Short).as_deref(), Some(w), "{o:?}");
    }
}

#[test]
fn a_context_overflow_keeps_its_compaction_hint() {
    let msg = "HTTP status: 400 maximum context length exceeded";
    let o = O::ProviderFailed {
        message: msg.into(),
    };
    assert_eq!(
        say(&o, Face::Short).unwrap(),
        format!("provider failed: {msg} (compaction will run on the next resume)")
    );
    // The operator log line never carried the hint.
    assert_eq!(
        say(&o, Face::Log).unwrap(),
        format!("tole: provider failed: {msg}")
    );
}

#[test]
fn a_final_a_cancel_and_a_detach_have_no_words() {
    let done = O::Final {
        text: "t".into(),
        wrote: false,
    };
    for o in [done, O::Cancelled, O::Detached] {
        assert_eq!(say(&o, Face::Log), None);
        assert_eq!(say(&o, Face::Short), None);
    }
}
