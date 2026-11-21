//! The answer flow: a click, a failed call, the wait running out.

use super::super::request::{Decision, DecisionIntent, Outcome};
use super::{AnswerError, ApprovalsState, Phase};

impl ApprovalsState {
    /// The user pressed a button. On `Ok` the adapter sends the intent; the
    /// card shows "sending" and no buttons until the server confirms with
    /// [`super::ApprovalsAction::Resolved`] or the call fails with
    /// [`super::ApprovalsAction::AnswerFailed`]. Refused when the request is
    /// not waiting, is being answered, or its time is up (it is then moved to
    /// Decided as Expired): a stale click never allows anything.
    pub fn answer(
        &mut self,
        id: &str,
        decision: Decision,
        now: u64,
    ) -> Result<DecisionIntent, AnswerError> {
        let e = self.entries.get_mut(id).ok_or(AnswerError::Unknown)?;
        if !matches!(e.phase, Phase::Waiting { answering: None }) {
            return Err(AnswerError::NotWaiting);
        }
        if e.wait.expired(now) {
            self.expire(now);
            self.follow_selection();
            return Err(AnswerError::Expired);
        }
        e.phase = Phase::Waiting {
            answering: Some(decision),
        };
        let fingerprint = e.request.fingerprint.clone();
        Ok(DecisionIntent {
            id: id.to_owned(),
            decision,
            fingerprint,
        })
    }

    /// Requests whose time is up are Decided as Expired, not yet confirmed by
    /// the server (a later `Resolved` replaces it). One whose answer was on
    /// its way is Expired too, and says the answer may not have arrived: the
    /// reply never came, and the server has expired it by now.
    pub(super) fn expire(&mut self, now: u64) -> bool {
        let mut changed = false;
        for e in self.entries.values_mut() {
            if let Phase::Waiting { answering } = e.phase {
                if e.wait.expired(now) {
                    e.phase = Phase::Decided {
                        outcome: Outcome::Expired,
                        at_ms: now,
                        provisional: true,
                        answer_lost: answering.is_some(),
                    };
                    changed = true;
                }
            }
        }
        let before = self.overflow.len();
        self.overflow.retain(|_, o| !o.wait.expired(now));
        changed |= self.overflow.len() != before;
        if changed {
            self.trim_decided();
        }
        changed
    }

    /// The decision call did not complete. The request waits again only if it
    /// is still pending and in time; a conflict makes the list stale.
    pub(super) fn answer_failed(&mut self, id: &str, conflict: bool, now: u64) -> bool {
        let mut changed = false;
        if let Some(e) = self.entries.get_mut(id) {
            if matches!(e.phase, Phase::Waiting { answering: Some(_) }) {
                e.phase = if e.wait.expired(now) {
                    Phase::Decided {
                        outcome: Outcome::Expired,
                        at_ms: now,
                        provisional: true,
                        answer_lost: false,
                    }
                } else {
                    Phase::Waiting { answering: None }
                };
                changed = true;
            }
        }
        if changed {
            self.trim_decided();
        }
        changed |= conflict && !std::mem::replace(&mut self.stale, true);
        changed
    }
}
