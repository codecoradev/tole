//! The colour meaning of the dot before the status sentence.

use super::status::{Phase, StatusRow};
use crate::tokens::Tone;

/// Going on and done read as good; waiting for the owner or stopping asks for
/// attention; a lost connection is bad; a task that has ended or not begun
/// claims nothing.
pub fn phase_tone(status: &StatusRow) -> Tone {
    match status.phase {
        Phase::Working | Phase::Resuming => Tone::Good,
        Phase::Idle if status.started => Tone::Good,
        Phase::Idle | Phase::Closed => Tone::Neutral,
        Phase::WaitingApproval | Phase::Stopping => Tone::Attention,
        Phase::Offline => Tone::Bad,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::task::{Counts, SendMode};

    #[rustfmt::skip]
    fn row(phase: Phase, started: bool) -> StatusRow {
        let counts = Counts::default();
        StatusRow { phase, counts, steps: 0, look_only: false, started, complete: true, can_stop: false, send: SendMode::Send }
    }

    #[test]
    fn the_dot_says_whether_the_owner_is_needed_or_something_is_wrong() {
        assert_eq!(phase_tone(&row(Phase::Working, true)), Tone::Good);
        assert_eq!(phase_tone(&row(Phase::Idle, true)), Tone::Good);
        assert_eq!(phase_tone(&row(Phase::Idle, false)), Tone::Neutral);
        assert_eq!(
            phase_tone(&row(Phase::WaitingApproval, true)),
            Tone::Attention
        );
        assert_eq!(phase_tone(&row(Phase::Stopping, true)), Tone::Attention);
        assert_eq!(phase_tone(&row(Phase::Offline, true)), Tone::Bad);
        assert_eq!(phase_tone(&row(Phase::Closed, true)), Tone::Neutral);
    }
}
