//! The exit summary of a turn.

use tole_core::turn::{TurnOutcome, LOOP_TRIP_AFTER};

// ---------------------------------------------------------------------------
// System prompt (B2)
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// Reporting
// ---------------------------------------------------------------------------

/// Human-readable exit summary. Non-Final outcomes exit non-zero so
/// scripts notice.
pub(crate) fn report_outcome(session_id: &str, outcome: TurnOutcome) {
    // How the run ended, for the skill curator (#593), before any exit below.
    tole_cli::skill_outcome::record_turn(&outcome);
    match outcome {
        TurnOutcome::Final { text, .. } => {
            println!("{text}");
        }
        TurnOutcome::StopGateBlocked { reason } => {
            eprintln!(
                "tole: stop gate blocked the turn: {reason} (resume with: tole resume {session_id})"
            );
            std::process::exit(6);
        }
        TurnOutcome::ApprovalRequired { name } => {
            eprintln!(
                "tole: approval denied for '{name}' — turn aborted, denial recorded \
                 (resume with: tole resume {session_id})"
            );
            std::process::exit(2);
        }
        TurnOutcome::UnknownTool { name } => {
            eprintln!("tole: unknown tool '{name}' — turn aborted, error recorded");
            std::process::exit(3);
        }
        TurnOutcome::ProviderFailed { message } => {
            eprintln!(
                "tole: provider failed: {message}{} (resume with: tole resume {session_id})",
                tole_cli::compaction_host::overflow_hint(&message)
            );
            std::process::exit(4);
        }
        TurnOutcome::BudgetExhausted => {
            eprintln!("tole: step budget exhausted (resume with: tole resume {session_id})");
            std::process::exit(5);
        }
        TurnOutcome::Cancelled => {
            eprintln!("tole: turn cancelled (resume with: tole resume {session_id})");
            std::process::exit(8);
        }
        TurnOutcome::Detached => {
            eprintln!("tole: turn detached (resume with: tole resume {session_id})");
            std::process::exit(9);
        }
        TurnOutcome::LoopDetected { tool, count } => {
            eprintln!(
                "tole: loop detected — tool `{tool}` called with identical input {count} times in a row (guard trips at {LOOP_TRIP_AFTER}); resume with: tole resume {session_id}"
            );
            std::process::exit(7);
        }
        TurnOutcome::Storage(e) => {
            eprintln!("tole: storage error: {e}");
            std::process::exit(6);
        }
    }
}
