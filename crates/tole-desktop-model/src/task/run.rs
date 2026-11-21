//! The live side of a task: is a turn running, what waits for an OK, is the
//! connection alive. Rules:
//!
//! - Durable entries never start a turn (a replayed history must not look
//!   busy). Working means: a prompt was sent, a `ready` snapshot says busy, or
//!   text or reasoning streams. The final answer, `TurnEnded` and a not-busy
//!   snapshot end it, except a snapshot while the owner's prompt is still going
//!   out (`sending`).
//! - Text arrives on SSE and the end of the turn on the prompt reply, so a text
//!   or reasoning frame after the turn ended (or after its answer) is late: it
//!   is ignored (`text_closed`) until a new prompt, a busy snapshot or an
//!   allow. After a reconnect or `resync` text is also ignored (`discard_text`)
//!   until the turn settles or a new prompt comes (a steer note does not count).
//! - A turn that asks for an OK is parked, not running: a request clears busy
//!   and stop. A denial leaves the turn parked, so it is idle. An allow resumes
//!   it with no one watching (`unobserved`): the phase is `Resuming` until a
//!   snapshot, text, a tool entry or an answer confirms it. A not-busy snapshot
//!   does not (serve starts the turn after its 202), unless Stop was pressed.
//!   Stop works while resuming, steering does not. `ResumeTimedOut` ends an
//!   unobserved turn whenever it is still running, since nothing else would.
//! - An approval is kept once per id. The refresh of the queue adds what the
//!   server lists, keeps what the stream announced since the stream (re)opened,
//!   and does not bring back what the stream resolved meanwhile. A request for
//!   an id already resolved is a late announcement and is ignored.
//! - `stopping` ends with the turn, a not-busy snapshot, a closed session, a
//!   failed prompt or a failed Stop.

use super::event::{ApprovalInfo, TaskEvent, UserKind, MAX_TOOL_CHARS};
use crate::plain::printable;

/// Ids remembered since the last refresh (a long session asks few approvals).
const MAX_TRACKED_IDS: usize = 256;

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
    /// The turn runs with no prompt call to wait for (resumed after an allow).
    pub unobserved: bool,
    /// The owner's prompt is going out (images upload, then the call).
    pub sending: bool,
    /// Stop was pressed and the turn has not ended.
    pub stopping: bool,
    pub closed: bool,
    /// The turn is over: later text frames are late, not new text.
    pub text_closed: bool,
    /// Text frames are stale (frames were lost) until the turn settles.
    pub discard_text: bool,
    /// Fetch the queue and send `ApprovalsRefreshed`.
    pub approvals_stale: bool,
    pub link: Link,
    /// Waiting for the owner, oldest first.
    pub approvals: Vec<ApprovalInfo>,
    /// Ids the stream announced, and resolved, since the last refresh.
    seen: Vec<String>,
    gone: Vec<String>,
}

fn track(ids: &mut Vec<String>, id: &str) {
    if !ids.iter().any(|i| i == id) {
        if ids.len() >= MAX_TRACKED_IDS {
            ids.remove(0);
        }
        ids.push(id.to_string());
    }
}

impl Run {
    /// Text and reasoning frames are part of a live turn right now.
    pub fn text_accepted(&self) -> bool {
        !self.text_closed && !self.discard_text
    }

    /// The owner sent a prompt.
    pub fn prompt_sent(&mut self) {
        self.busy = true;
        self.sending = true;
        self.discard_text = false;
        self.text_closed = false;
    }

    /// The prompt was refused or failed; `server_busy` is true for a 409.
    pub fn prompt_failed(&mut self, server_busy: bool) {
        self.sending = false;
        self.busy = server_busy;
        self.stopping &= server_busy;
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
        self.unobserved = false;
        self.sending = false;
        self.stopping = false;
        self.discard_text = false;
        self.text_closed = true;
    }

    fn remember(&mut self, info: &ApprovalInfo) {
        let info = info.cleaned();
        track(&mut self.seen, &info.id);
        if !self.approvals.iter().any(|a| a.id == info.id) {
            self.approvals.push(info);
        }
    }

    pub fn apply(&mut self, event: &TaskEvent) {
        match event {
            TaskEvent::UserMessage {
                kind: UserKind::Prompt,
                ..
            } => {
                self.discard_text = false;
                self.text_closed = false;
                self.sending = false;
            }
            TaskEvent::AnswerEntry { .. } | TaskEvent::TurnEnded => self.end_turn(),
            TaskEvent::TextDelta { .. } | TaskEvent::Reasoning => {
                if self.text_accepted() {
                    self.busy = true;
                    self.resuming = false;
                }
            }
            TaskEvent::ToolIntent { .. } | TaskEvent::ToolSettled { .. } => self.resuming = false,
            TaskEvent::Ready { busy: true } => {
                self.link = Link::Live;
                self.busy = true;
                self.resuming = false;
                self.text_closed = false;
            }
            TaskEvent::Ready { busy: false } => {
                self.link = Link::Live;
                // Not yet running is not over: the prompt is still going out, or an
                // allowed turn is being started (serve answers 202 first). Only a
                // Stop makes the owner's "nothing runs" final.
                if self.stopping || !(self.sending || self.resuming) {
                    self.end_turn();
                }
            }
            TaskEvent::ApprovalRequested(info) => {
                self.link = Link::Live;
                if self.gone.contains(&info.cleaned().id) {
                    return; // already decided: a late announcement, not a new card
                }
                self.busy = false;
                self.resuming = false;
                self.unobserved = false;
                self.sending = false;
                self.stopping = false;
                self.text_closed = true;
                self.remember(info);
            }
            TaskEvent::ApprovalResolved { id, approved } => {
                let id = printable(id, MAX_TOOL_CHARS);
                track(&mut self.gone, &id);
                let before = self.approvals.len();
                self.approvals.retain(|a| a.id != id);
                if self.approvals.len() < before && self.approvals.is_empty() {
                    if *approved {
                        self.busy = true;
                        self.resuming = true;
                        self.unobserved = true;
                        self.text_closed = false;
                    } else {
                        self.end_turn();
                    }
                }
            }
            TaskEvent::ApprovalsRefreshed { pending } => {
                let announced = std::mem::take(&mut self.seen);
                let decided = std::mem::take(&mut self.gone);
                let kept: Vec<_> = (self.approvals.iter())
                    .filter(|a| announced.contains(&a.id))
                    .cloned()
                    .collect();
                let listed = pending.iter().map(ApprovalInfo::cleaned);
                let listed: Vec<_> = listed.filter(|a| !decided.contains(&a.id)).collect();
                self.approvals.clear();
                listed
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
                self.gone.clear();
            }
            TaskEvent::Resync => self.discard_text = true,
            TaskEvent::ConnectionLost => self.link = Link::Lost,
            TaskEvent::ResumeTimedOut if self.unobserved => self.end_turn(),
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
        r.apply(&TaskEvent::Ready { busy: true });
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
