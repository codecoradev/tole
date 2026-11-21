//! The task view's state (#1031, part of #527), tested without a window:
//! [`TaskEvent`] (what the client yields, with the adapter's mapping rules),
//! [`Transcript`], [`ListLayout`] and [`visible`] (which rows of a long
//! transcript to build), [`StatusRow`] and [`SendMode`] (the status sentence,
//! "Look only" chip, Stop and steer rules), [`Composer`], and [`TaskState`],
//! which ties them together. The words are in [`crate::strings::task`].

mod composer;
mod event;
mod run;
mod status;
mod transcript;
#[cfg(test)]
mod transcript_tests;
mod viewport;

pub use composer::{
    AttachError, Attachment, AttachmentId, Composer, Enter, EnterOutcome, FolderChip, Submission,
    MAX_IMAGE_BYTES,
};
pub use event::{tool_effect, ApprovalInfo, PlanItem, PlanStatus, TaskEvent, ToolEffect};
pub use run::{Link, Run};
pub use status::{status_row, Phase, SendMode, StatusRow};
pub use transcript::{Counts, Item, Step, StepState, Transcript, WorkGroup};
pub use viewport::{visible, ListLayout, Visible};

/// One open task.
#[derive(Debug, Clone)]
pub struct TaskState {
    /// What to list. Read it; change it only through [`TaskState::apply`].
    pub transcript: Transcript,
    /// Whether a turn runs, what waits for an OK; read-only like `transcript`.
    pub run: Run,
    /// Row heights; the window sets them with `set_height` for rows it built.
    pub layout: ListLayout,
    pub composer: Composer,
    /// Opened in plan mode (`SessionOpened::plan_mode`); fixed for the session.
    pub look_only: bool,
}

impl TaskState {
    pub fn new(look_only: bool) -> Self {
        TaskState {
            transcript: Transcript::default(),
            run: Run::default(),
            layout: ListLayout::new(64.0), /* assumed row height, px */
            composer: Composer::default(),
            look_only,
        }
    }

    /// Fold in one event, from a history page or the live stream.
    pub fn apply(&mut self, event: TaskEvent) {
        let stale = matches!(event, TaskEvent::TextDelta { .. }) && self.run.discard_text;
        if !stale {
            self.run.apply(&event);
            self.transcript.apply(&event);
            self.layout.set_len(self.transcript.items.len());
        }
    }

    pub fn status(&self) -> StatusRow {
        status_row(&self.transcript, &self.run, self.look_only)
    }

    /// Row `index` is a "Working" group that is going on: one of its calls
    /// runs, or it is the last row of a running turn.
    pub fn group_active(&self, index: usize) -> bool {
        let last = index + 1 == self.transcript.items.len();
        match self.transcript.items.get(index) {
            Some(Item::Work(g)) => g.running() || (last && self.run.busy),
            _ => false,
        }
    }

    pub fn toggle_group(&mut self, index: usize) {
        self.transcript.toggle_group(index);
    }

    /// An Enter press in the composer. A prompt marks the turn as running at
    /// once; the adapter then uploads the images and calls the prompt route
    /// (the interrupt route for a steer note), and calls
    /// [`TaskState::send_failed`] if that fails.
    pub fn press_enter(&mut self, key: Enter) -> EnterOutcome {
        let outcome = self.composer.enter(key, self.status().send);
        if matches!(&outcome, EnterOutcome::Submit(s) if s.mode == SendMode::Send) {
            self.run.prompt_sent();
        }
        outcome
    }

    /// The prompt or steer call failed: the draft comes back, and a prompt no
    /// longer counts as running.
    pub fn send_failed(&mut self, sent: Submission) {
        if sent.mode == SendMode::Send {
            self.run.prompt_failed();
        }
        self.composer.restore(sent);
    }

    /// Stop was pressed. True when the adapter should call the cancel route:
    /// only for a running turn, once.
    pub fn press_stop(&mut self) -> bool {
        let can = self.status().can_stop;
        if can {
            self.run.stop_requested();
        }
        can
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[rustfmt::skip]
    const ENTER: Enter = Enter { shift: false, composing: false };

    fn sent(t: &mut TaskState, text: &str) -> Submission {
        t.composer.text = text.into();
        match t.press_enter(ENTER) {
            EnterOutcome::Submit(s) => s,
            other => panic!("not sent: {other:?}"),
        }
    }

    #[test]
    fn a_prompt_marks_the_turn_running_and_stop_works_once_and_a_failure_restores() {
        let mut t = TaskState::new(false);
        let s = sent(&mut t, "tidy");
        assert_eq!((s.mode, t.status().phase), (SendMode::Send, Phase::Working));
        assert!(t.press_stop() && !t.press_stop());
        assert_eq!(t.status().phase, Phase::Stopping);
        t.apply(TaskEvent::TurnEnded);
        t.send_failed(s);
        assert_eq!(
            (t.composer.text.as_str(), t.status().phase),
            ("tidy", Phase::Idle)
        );
    }

    #[test]
    fn text_after_a_reconnect_is_not_shown_and_the_final_entry_is() {
        let mut t = TaskState::new(false);
        t.apply(TaskEvent::TextDelta { text: "Hel".into() });
        t.apply(TaskEvent::Reconnected);
        t.apply(TaskEvent::TextDelta { text: "lo".into() });
        assert!(t.transcript.items.is_empty());
        t.apply(TaskEvent::AnswerEntry {
            seq: 7,
            text: "Hello.".into(),
        });
        assert_eq!(t.transcript.items.len(), 1);
    }

    #[test]
    fn a_group_is_active_while_a_call_runs_or_the_turn_ends_on_it() {
        let mut t = TaskState::new(false);
        let (id, tool) = ("e1".to_string(), "read_file".to_string());
        t.apply(TaskEvent::ToolIntent {
            seq: 1,
            id: id.clone(),
            tool,
            target: None,
        });
        assert!(t.group_active(0));
        t.apply(TaskEvent::ToolSettled {
            seq: 2,
            intent_id: id,
            ok: true,
        });
        assert!(!t.group_active(0));
        t.apply(TaskEvent::Ready { busy: true });
        assert!(t.group_active(0) && !t.group_active(5));
    }
}
