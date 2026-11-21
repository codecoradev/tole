//! The status row, and what the owner may do now. [`status_row`] derives it
//! all from the transcript and the run state; the first rule that fits wins:
//!
//! | Phase | Stop | Enter in the composer |
//! |---|---|---|
//! | Closed, Offline | no | disabled |
//! | Waiting for an OK | no (the card answers it) | disabled |
//! | Stopping | no (already asked) | disabled |
//! | Working | yes | steers |
//! | Idle | no | sends a prompt |

use super::run::{Link, Run};
use super::transcript::{Counts, Transcript};

/// Where the task is; the first rule that fits wins, top to bottom.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    Idle,
    Working,
    WaitingApproval,
    Stopping,
    /// The event stream is lost.
    Offline,
    /// The session left the server.
    Closed,
}

/// What Enter does in the composer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SendMode {
    /// A new prompt.
    Send,
    /// A note to the running turn (the interrupt route).
    Steer,
    /// Nothing is sent; the phase says why.
    Disabled(Phase),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StatusRow {
    pub phase: Phase,
    pub counts: Counts,
    /// Tool calls so far (plans excluded).
    pub steps: usize,
    /// Opened in plan mode (no tool can change anything); fixed per session.
    pub look_only: bool,
    /// The task has at least one row.
    pub started: bool,
    pub can_stop: bool,
    pub send: SendMode,
}

pub fn status_row(transcript: &Transcript, run: &Run, look_only: bool) -> StatusRow {
    let phase = if run.closed {
        Phase::Closed
    } else if run.link == Link::Lost {
        Phase::Offline
    } else if !run.approvals.is_empty() {
        Phase::WaitingApproval
    } else if run.stopping {
        Phase::Stopping
    } else if run.busy {
        Phase::Working
    } else {
        Phase::Idle
    };
    let send = match phase {
        Phase::Idle => SendMode::Send,
        Phase::Working => SendMode::Steer,
        other => SendMode::Disabled(other),
    };
    StatusRow {
        phase,
        counts: transcript.counts,
        steps: transcript.step_count(),
        look_only,
        started: !transcript.items.is_empty(),
        can_stop: phase == Phase::Working,
        send,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::task::event::{ApprovalInfo, TaskEvent};

    fn row(events: &[TaskEvent], prompt: bool) -> StatusRow {
        let (mut t, mut r) = (Transcript::default(), Run::default());
        if prompt {
            r.prompt_sent();
        }
        for e in events {
            t.apply(e);
            r.apply(e);
        }
        status_row(&t, &r, false)
    }

    #[rustfmt::skip]
    fn ask() -> TaskEvent {
        let info = ApprovalInfo { id: "a1".into(), tool: "write_file".into(), summary: None };
        TaskEvent::ApprovalRequested(info)
    }

    #[test]
    fn idle_sends_working_steers_and_can_stop_and_waiting_blocks_both() {
        let idle = row(&[], false);
        assert_eq!(
            (idle.phase, idle.send, idle.can_stop),
            (Phase::Idle, SendMode::Send, false)
        );
        let working = row(&[], true);
        assert_eq!((working.send, working.can_stop), (SendMode::Steer, true));
        let waiting = row(&[ask()], true);
        assert_eq!(waiting.send, SendMode::Disabled(Phase::WaitingApproval));
        assert!(!waiting.can_stop);
    }

    #[test]
    fn a_lost_connection_or_a_closed_session_blocks_everything() {
        let lost = row(&[ask(), TaskEvent::ConnectionLost], true);
        assert_eq!((lost.phase, lost.can_stop), (Phase::Offline, false));
        let closed = row(&[TaskEvent::Closed], true);
        assert_eq!(closed.send, SendMode::Disabled(Phase::Closed));
    }
}
