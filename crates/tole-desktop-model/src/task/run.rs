//! The live side of a task: is a turn running, what waits for an OK, is the
//! connection alive. Rules:
//!
//! - Durable entries never start a turn (a replayed history must not look
//!   busy). Working means: a prompt was sent, a `ready` snapshot says busy,
//!   text or reasoning streams, or an approval was allowed. The final answer,
//!   `TurnEnded` and a not-busy snapshot end it.
//! - An approval is kept once per id; a reconnect marks the queue stale
//!   (a decision made while disconnected is never announced) until
//!   `ApprovalsRefreshed`.
//! - Streamed text after a reconnect or `resync` is discarded until the turn
//!   settles: frames were lost.

use super::event::{ApprovalInfo, TaskEvent};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Link {
    #[default]
    Live,
    /// The stream is over and does not come back by itself.
    Lost,
}

#[derive(Debug, Clone, Default)]
pub struct Run {
    /// A turn is running (one waiting for an OK is not).
    pub busy: bool,
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
}

impl Run {
    /// The owner sent a prompt.
    pub fn prompt_sent(&mut self) {
        self.busy = true;
        self.discard_text = false;
    }

    /// The prompt could not be sent.
    pub fn prompt_failed(&mut self) {
        self.busy = false;
    }

    /// Stop was pressed; only a running turn can be stopped.
    pub fn stop_requested(&mut self) {
        self.stopping |= self.busy;
    }

    fn end_turn(&mut self) {
        self.busy = false;
        self.stopping = false;
        self.discard_text = false;
    }

    fn remember(&mut self, info: &ApprovalInfo) {
        if !self.approvals.iter().any(|a| a.id == info.id) {
            self.approvals.push(info.clone());
        }
    }

    pub fn apply(&mut self, event: &TaskEvent) {
        match event {
            TaskEvent::UserMessage { .. } => self.discard_text = false,
            TaskEvent::AnswerEntry { .. } | TaskEvent::TurnEnded => self.end_turn(),
            TaskEvent::TextDelta { .. } | TaskEvent::Reasoning => self.busy = true,
            TaskEvent::Ready { busy } => {
                self.link = Link::Live;
                if *busy {
                    self.busy = true;
                } else {
                    self.end_turn();
                }
            }
            TaskEvent::ApprovalRequested(info) => {
                self.link = Link::Live;
                self.remember(info);
            }
            TaskEvent::ApprovalResolved { id, approved } => {
                self.approvals.retain(|a| a.id != *id);
                self.busy |= *approved;
            }
            TaskEvent::ApprovalsRefreshed { pending } => {
                self.approvals.clear();
                pending.iter().for_each(|info| self.remember(info));
                self.approvals_stale = false;
            }
            TaskEvent::Reconnected => {
                self.link = Link::Live;
                self.approvals_stale = true;
                self.discard_text = true;
            }
            TaskEvent::Resync => self.discard_text = true,
            TaskEvent::ConnectionLost => self.link = Link::Lost,
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

    #[test]
    fn an_approval_repeated_after_a_reconnect_is_kept_once_and_a_refresh_replaces_it() {
        let req = TaskEvent::ApprovalRequested(approval("a1"));
        let mut r = run(&[req.clone(), TaskEvent::Reconnected, req]);
        assert!(r.approvals.len() == 1 && r.approvals_stale);
        let pending = vec![approval("a2"), approval("a2")];
        r.apply(&TaskEvent::ApprovalsRefreshed { pending });
        assert_eq!(r.approvals, [approval("a2")]);
        assert!(!r.approvals_stale);
    }

    #[test]
    fn an_allow_resumes_the_turn_and_a_denial_does_not() {
        let req = TaskEvent::ApprovalRequested(approval("a1"));
        let denied = run(&[req.clone(), resolved(false)]);
        assert!(denied.approvals.is_empty() && !denied.busy);
        assert!(run(&[req, resolved(true)]).busy);
    }
}
