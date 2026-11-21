//! The live side of a task: is a turn running, what waits for an OK, is the
//! connection alive. Rules:
//!
//! - Durable entries never start a turn (a replayed history must not look
//!   busy). Working means: a prompt was sent, a `ready` snapshot says busy, or
//!   text or reasoning streams. The final answer, `TurnEnded` and a not-busy
//!   snapshot end it.
//! - A turn that asks for an OK is parked, not running: a request clears busy
//!   and stop. A denial leaves the turn parked, so it is idle. An allow resumes
//!   it with no one watching: the phase is `Resuming` until a snapshot, text,
//!   a tool entry, an answer or `ResumeTimedOut` settles it. Stop works while
//!   resuming, steering does not.
//! - An approval is kept once per id. A reconnect marks the queue stale
//!   (a decision made while disconnected is never announced); the refresh adds
//!   what the server lists and keeps what the stream announced after the
//!   reconnect, so an approval asked after the fetch is not lost.
//! - Streamed text after a reconnect or `resync` is discarded until the turn
//!   settles or the owner sends a new prompt (not a steer note).
//! - `stopping` ends with the turn, a not-busy snapshot, a closed session or a
//!   failed Stop.

use super::event::{ApprovalInfo, TaskEvent, UserKind};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Link {
    #[default]
    Live,
    /// The stream is over and does not come back by itself.
    Lost,
}

#[derive(Debug, Clone, Default)]
pub struct Run {
    /// A turn is running or resuming (one waiting for an OK is not).
    pub busy: bool,
    /// An allowed turn is not confirmed running yet; `busy` is then true.
    pub resuming: bool,
    /// Stop was pressed and the turn has not ended.
    pub stopping: bool,
    pub closed: bool,
    /// Text frames are not to be shown now.
    pub discard_text: bool,
    /// Fetch the queue and send `ApprovalsRefreshed`.
    pub approvals_stale: bool,
    pub link: Link,
    /// Waiting for the owner, oldest first.
    pub approvals: Vec<ApprovalInfo>,
    /// Ids the stream announced since the reconnect (while the queue is stale).
    seen: Vec<String>,
}

impl Run {
    /// The owner sent a prompt.
    pub fn prompt_sent(&mut self) {
        self.busy = true;
        self.discard_text = false;
    }

    /// The prompt was refused or failed; `server_busy` is true for a 409.
    pub fn prompt_failed(&mut self, server_busy: bool) {
        self.busy = server_busy;
        self.resuming &= server_busy;
    }

    /// Stop was pressed; only a running turn can be stopped.
    pub fn stop_requested(&mut self) {
        self.stopping |= self.busy;
    }

    /// The Stop call failed: the turn runs on.
    pub fn stop_failed(&mut self) {
        self.stopping = false;
    }

    fn end_turn(&mut self) {
        self.busy = false;
        self.resuming = false;
        self.stopping = false;
        self.discard_text = false;
    }

    fn remember(&mut self, info: &ApprovalInfo) {
        let info = info.cleaned();
        if self.approvals_stale && !self.seen.contains(&info.id) {
            self.seen.push(info.id.clone());
        }
        if !self.approvals.iter().any(|a| a.id == info.id) {
            self.approvals.push(info);
        }
    }

    pub fn apply(&mut self, event: &TaskEvent) {
        match event {
            TaskEvent::UserMessage {
                kind: UserKind::Prompt,
                ..
            } => self.discard_text = false,
            TaskEvent::AnswerEntry { .. } | TaskEvent::TurnEnded => self.end_turn(),
            TaskEvent::TextDelta { .. } | TaskEvent::Reasoning => {
                self.busy = true;
                self.resuming = false;
            }
            TaskEvent::ToolIntent { .. } | TaskEvent::ToolSettled { .. } => self.resuming = false,
            TaskEvent::Ready { busy: true } => {
                self.link = Link::Live;
                self.busy = true;
                self.resuming = false;
            }
            TaskEvent::Ready { busy: false } => {
                self.link = Link::Live;
                self.end_turn();
            }
            TaskEvent::ApprovalRequested(info) => {
                self.link = Link::Live;
                self.busy = false;
                self.resuming = false;
                self.stopping = false;
                self.remember(info);
            }
            TaskEvent::ApprovalResolved { id, approved } => {
                let before = self.approvals.len();
                self.approvals.retain(|a| a.id != *id);
                if self.approvals.len() < before && self.approvals.is_empty() {
                    if *approved {
                        self.busy = true;
                        self.resuming = true;
                    } else {
                        self.end_turn();
                    }
                }
            }
            TaskEvent::ApprovalsRefreshed { pending } => {
                let announced = std::mem::take(&mut self.seen);
                let kept: Vec<_> = (self.approvals.iter())
                    .filter(|a| announced.contains(&a.id))
                    .cloned()
                    .collect();
                self.approvals.clear();
                pending
                    .iter()
                    .chain(&kept)
                    .for_each(|info| self.remember(info));
                self.approvals_stale = false;
                self.seen.clear();
            }
            TaskEvent::Reconnected => {
                self.link = Link::Live;
                self.approvals_stale = true;
                self.discard_text = true;
                self.seen.clear();
            }
            TaskEvent::Resync => self.discard_text = true,
            TaskEvent::ConnectionLost => self.link = Link::Lost,
            TaskEvent::ResumeTimedOut if self.resuming => self.end_turn(),
            TaskEvent::Closed => {
                self.closed = true;
                self.end_turn();
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[rustfmt::skip]
    fn approval(id: &str) -> ApprovalInfo {
        ApprovalInfo { id: id.into(), tool: "write_file".into(), summary: None }
    }

    fn run(events: &[TaskEvent]) -> Run {
        let mut r = Run::default();
        events.iter().for_each(|e| r.apply(e));
        r
    }

    #[rustfmt::skip]
    fn resolved(approved: bool) -> TaskEvent {
        TaskEvent::ApprovalResolved { id: "a1".into(), approved }
    }

    #[test]
    fn history_does_not_make_a_task_busy_but_a_snapshot_can_and_the_answer_ends_it() {
        let (id, tool) = ("e1".to_string(), "read_file".to_string());
        let intent = TaskEvent::ToolIntent {
            seq: 1,
            id,
            tool,
            target: None,
        };
        let (busy, end) = (TaskEvent::Ready { busy: true }, TaskEvent::TurnEnded);
        assert!(!run(std::slice::from_ref(&intent)).busy);
        assert!(run(&[intent, busy.clone()]).busy);
        assert!(!run(&[busy, end]).busy);
    }

    #[rustfmt::skip]
    fn user(kind: UserKind) -> TaskEvent {
        TaskEvent::UserMessage { seq: 1, text: "x".into(), images: 0, kind }
    }

    #[test]
    fn a_refresh_keeps_what_the_stream_announced_after_the_reconnect() {
        let ask = |id| TaskEvent::ApprovalRequested(approval(id));
        let mut r = run(&[
            ask("old"),
            ask("a1"),
            TaskEvent::Reconnected,
            ask("a1"),
            ask("late"),
        ]);
        assert!(r.approvals.len() == 3 && r.approvals_stale);
        let pending = vec![approval("a2"), approval("a2")];
        r.apply(&TaskEvent::ApprovalsRefreshed { pending });
        let ids: Vec<_> = r.approvals.iter().map(|a| a.id.as_str()).collect();
        assert_eq!(
            ids,
            ["a2", "a1", "late"],
            "`old` was decided while disconnected"
        );
        assert!(!r.approvals_stale);
    }

    #[test]
    fn an_allow_resumes_unconfirmed_and_a_denial_ends_the_parked_turn() {
        let req = TaskEvent::ApprovalRequested(approval("a1"));
        let denied = run(&[
            TaskEvent::Ready { busy: true },
            req.clone(),
            resolved(false),
        ]);
        assert!(denied.approvals.is_empty() && !denied.busy && !denied.resuming);
        let allowed = run(&[req.clone(), resolved(true)]);
        assert!(allowed.busy && allowed.resuming);
        // Text, a snapshot, a tool entry, an answer or the timeout settle it.
        let settles = [
            TaskEvent::Reasoning,
            TaskEvent::Ready { busy: true },
            TaskEvent::TurnEnded,
        ];
        for event in settles {
            let r = run(&[req.clone(), resolved(true), event.clone()]);
            assert!(!r.resuming, "{event:?}");
        }
        let gave_up = run(&[req, resolved(true), TaskEvent::ResumeTimedOut]);
        assert!(!gave_up.busy && !gave_up.resuming);
    }

    #[test]
    fn a_request_parks_the_turn_and_stop_ends_with_the_turn_or_its_failure() {
        let mut r = run(&[TaskEvent::Ready { busy: true }]);
        r.stop_requested();
        assert!(r.stopping);
        r.stop_failed();
        assert!(!r.stopping && r.busy);
        r.stop_requested();
        r.apply(&TaskEvent::ApprovalRequested(approval("a1")));
        assert!(!r.busy && !r.stopping);
        r.stop_requested();
        assert!(!r.stopping, "nothing runs to be stopped");
        r.prompt_sent();
        r.stop_requested();
        r.apply(&TaskEvent::Ready { busy: false });
        assert!(!r.stopping && !r.busy);
    }

    #[test]
    fn a_failed_prompt_stays_busy_only_when_serve_said_busy() {
        let (mut a, mut b) = (Run::default(), Run::default());
        a.prompt_sent();
        a.prompt_failed(false);
        b.prompt_sent();
        b.prompt_failed(true);
        assert!(!a.busy && b.busy);
    }

    #[test]
    fn only_the_owners_prompt_lets_text_through_again_and_approvals_are_made_printable() {
        let (steer, prompt) = (user(UserKind::Steer), user(UserKind::Prompt));
        assert!(run(&[TaskEvent::Resync, steer]).discard_text);
        assert!(!run(&[TaskEvent::Resync, prompt]).discard_text);
        let evil = ApprovalInfo {
            id: "a".into(),
            tool: "x\u{202E}y\n".into(),
            summary: Some("a\nb".into()),
        };
        let r = run(&[TaskEvent::ApprovalRequested(evil)]);
        assert_eq!(r.approvals[0].tool, "xy");
        assert_eq!(r.approvals[0].summary.as_deref(), Some("a b"));
    }
}
