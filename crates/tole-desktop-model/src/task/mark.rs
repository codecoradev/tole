//! The mark at the foot of a running transcript: is Tole thinking or writing?
//!
//! The model streams reasoning (text not kept) and answer text; between them
//! and a tool call there are quiet moments in which a list that shows nothing
//! looks dead. While a turn runs, the last row says which mark to show: a draft
//! being written is "writing", a call that runs is already a "Working" group,
//! and everything else (a prompt just sent, a call just settled, reasoning) is
//! "thinking". A turn that is not running, waits for an OK or is stopping shows
//! no mark: the status row says so.

use super::status::{Phase, StatusRow};
use super::transcript::Item;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mark {
    Thinking,
    Writing,
}

/// The mark for a task in `status` whose last row is `last` (`None` for an
/// empty transcript, or when the last row is not on screen).
pub fn mark(status: &StatusRow, last: Option<&Item>) -> Option<Mark> {
    if !matches!(status.phase, Phase::Working | Phase::Resuming) {
        return None;
    }
    match last {
        Some(Item::Answer {
            streaming: true, ..
        }) => Some(Mark::Writing),
        Some(Item::Work(g)) if g.running() => None,
        _ => Some(Mark::Thinking),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::task::{SendMode, Step, StepState, ToolEffect, WorkGroup};

    #[rustfmt::skip]
    fn status(phase: Phase) -> StatusRow {
        let counts = crate::task::Counts::default();
        StatusRow { phase, counts, steps: 0, look_only: false, started: true, complete: true, can_stop: false, send: SendMode::Send }
    }

    fn work(state: StepState) -> Item {
        let step = Step {
            id: "e1".into(),
            tool: "read_file".into(),
            target: None,
            effect: ToolEffect::Read,
            state,
        };
        Item::Work(WorkGroup {
            steps: vec![step],
            expanded: false,
        })
    }

    #[test]
    fn a_running_turn_is_thinking_until_text_comes_and_a_running_call_speaks_for_itself() {
        let working = status(Phase::Working);
        assert_eq!(mark(&working, None), Some(Mark::Thinking));
        let user = Item::User {
            text: "hi".into(),
            images: 0,
            steer: false,
        };
        assert_eq!(mark(&working, Some(&user)), Some(Mark::Thinking));
        assert_eq!(
            mark(&working, Some(&work(StepState::Done))),
            Some(Mark::Thinking)
        );
        assert_eq!(mark(&working, Some(&work(StepState::Running))), None);
        let draft = Item::Answer {
            markdown: "Hel".into(),
            streaming: true,
            cut: false,
        };
        assert_eq!(
            mark(&status(Phase::Resuming), Some(&draft)),
            Some(Mark::Writing)
        );
    }

    #[test]
    fn no_mark_unless_a_turn_runs() {
        for phase in [
            Phase::Idle,
            Phase::WaitingApproval,
            Phase::Stopping,
            Phase::Offline,
            Phase::Closed,
        ] {
            assert_eq!(mark(&status(phase), None), None, "{phase:?}");
        }
        let done = Item::Answer {
            markdown: "Done".into(),
            streaming: false,
            cut: false,
        };
        assert_eq!(
            mark(&status(Phase::Working), Some(&done)),
            Some(Mark::Thinking)
        );
    }
}
