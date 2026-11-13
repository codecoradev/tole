use super::*;
use tole_core::skill_curator::{RunTracker, UsageLedger};

fn ledger(tag: &str) -> UsageLedger {
    let d = std::env::temp_dir().join(format!("tole-outcome-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    UsageLedger::new(d.join("usage.jsonl"))
}

#[test]
fn a_turn_counts_when_it_settled_or_failed_and_not_when_it_was_interrupted() {
    let fin = TurnOutcome::Final {
        text: "x".into(),
        wrote: false,
    };
    assert_eq!(classify_turn(&fin), Outcome::Success);
    for failed in [
        TurnOutcome::ProviderFailed {
            message: "m".into(),
        },
        TurnOutcome::BudgetExhausted,
        TurnOutcome::StopGateBlocked { reason: "r".into() },
        TurnOutcome::UnknownTool { name: "t".into() },
        TurnOutcome::LoopDetected {
            tool: "t".into(),
            count: 3,
        },
    ] {
        assert_eq!(classify_turn(&failed), Outcome::Failure, "{failed:?}");
    }
    for skipped in [
        TurnOutcome::ApprovalRequired { name: "w".into() },
        TurnOutcome::Cancelled,
        TurnOutcome::Detached,
    ] {
        assert_eq!(classify_turn(&skipped), Outcome::Skip, "{skipped:?}");
    }
}

#[test]
fn a_mission_status_maps_to_success_failure_or_nothing() {
    assert_eq!(classify_mission("complete"), Outcome::Success);
    for f in [
        "verify_failed",
        "error",
        "exhausted_steps",
        "exhausted_tokens",
        "exhausted_time",
    ] {
        assert_eq!(classify_mission(f), Outcome::Failure, "{f}");
    }
    for s in ["stopped", "", "anything-else"] {
        assert_eq!(classify_mission(s), Outcome::Skip, "{s}");
    }
}

#[test]
fn the_outcome_goes_to_what_the_run_loaded_and_a_skip_writes_nothing() {
    let l = ledger("rec");
    let t = RunTracker::default();
    record_with(Outcome::Success, &t, &l, 10).unwrap();
    t.note("deploy");
    record_with(Outcome::Failure, &t, &l, 20).unwrap();
    record_with(Outcome::Skip, &t, &l, 30).unwrap();
    let s = l.summary();
    assert_eq!(
        (s.baseline.success, s.baseline.failure),
        (1, 0),
        "the run with no skill"
    );
    assert_eq!(
        (s.skills["deploy"].success, s.skills["deploy"].failure),
        (0, 1)
    );
    assert_eq!(
        s.skills["deploy"].last_used, 20,
        "the skipped run left no line"
    );
}
