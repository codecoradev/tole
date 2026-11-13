//! Run outcomes for the skill curator (#593): when `tole run` or `tole
//! mission` ends, how it ended is written to the usage ledger against each
//! skill the run loaded, or as the skill-less baseline when it loaded none.
//! The curator compares the two; it never acts on the comparison by itself.
//!
//! Only an end the skills could be blamed or credited for counts. A run
//! that stopped for an approval, was cancelled or was detached says nothing
//! about them and is not recorded.

use tole_core::skill_curator::{record_outcome, RunTracker, UsageLedger};
use tole_core::turn::TurnOutcome;

/// How a run ended, as far as the skills it used are concerned.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    Success,
    Failure,
    /// Interrupted or waiting: not recorded.
    Skip,
}

/// A turn's outcome.
pub fn classify_turn(o: &TurnOutcome) -> Outcome {
    match o {
        TurnOutcome::Final { .. } => Outcome::Success,
        TurnOutcome::ProviderFailed { .. }
        | TurnOutcome::BudgetExhausted
        | TurnOutcome::StopGateBlocked { .. }
        | TurnOutcome::LoopDetected { .. }
        | TurnOutcome::UnknownTool { .. }
        | TurnOutcome::Storage(_) => Outcome::Failure,
        _ => Outcome::Skip,
    }
}

/// A mission's final `status` (the word `tole mission` prints).
pub fn classify_mission(status: &str) -> Outcome {
    match status {
        "complete" => Outcome::Success,
        "verify_failed" | "error" => Outcome::Failure,
        s if s.starts_with("exhausted_") => Outcome::Failure,
        _ => Outcome::Skip,
    }
}

/// Write `outcome` for what `tracker` saw loaded. `Skip` writes nothing.
pub fn record_with(
    outcome: Outcome,
    tracker: &RunTracker,
    ledger: &UsageLedger,
    now: u64,
) -> Result<(), String> {
    let ok = match outcome {
        Outcome::Success => true,
        Outcome::Failure => false,
        Outcome::Skip => return Ok(()),
    };
    record_outcome(ledger, &tracker.loaded(), ok, now)
}

fn record(outcome: Outcome) {
    // Only where the curator has something to manage: a plain run writes
    // nothing.
    let Some(ledger) = UsageLedger::default_if_curated() else {
        return;
    };
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    // Bookkeeping: a ledger that cannot be written never changes how a run
    // ends.
    let _ = record_with(outcome, RunTracker::global(), &ledger, now);
}

/// Record the outcome of `tole run` (and `resume`).
pub fn record_turn(o: &TurnOutcome) {
    record(classify_turn(o));
}

/// Record the outcome of `tole mission`.
pub fn record_mission(status: &str) {
    record(classify_mission(status));
}

#[cfg(test)]
#[path = "skill_outcome_tests.rs"]
mod tests;
