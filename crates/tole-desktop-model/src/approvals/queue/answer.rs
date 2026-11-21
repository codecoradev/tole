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
            self.drop_selection();
            self.refresh_if_room();
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
        let mut n = self.decided_seq;
        for e in self.entries.values_mut() {
            match e.phase {
                Phase::Waiting { answering } if e.wait.expired(now) => {
                    n += 1;
                    e.phase = Phase::Decided {
                        outcome: Outcome::Expired,
                        rank: (true, n),
                        provisional: true,
                        answer_lost: answering.is_some(),
                    };
                    changed = true;
                }
                // Its time is over at the server too: it is no longer pending,
                // whatever status this version could not read.
                Phase::Decided {
                    provisional: true, ..
                } if e.wait.expired(now) && !Outcome::status_is_known(&e.request.status) => {
                    e.request.status = "closed".to_owned();
                    changed = true;
                }
                _ => {}
            }
        }
        self.decided_seq = n;
        let before = self.overflow.len();
        self.overflow.retain(|_, o| !o.wait.expired(now));
        changed |= self.overflow.len() != before;
        if changed {
            self.trim_decided();
        }
        changed
    }

    /// The decision call did not complete. The request waits again only if it
    /// is still pending and in time; a conflict (someone answered first) makes
    /// it "no longer waiting" instead, with no buttons, until the server's
    /// word or the next snapshot says otherwise; and makes the list stale.
    pub(super) fn answer_failed(&mut self, id: &str, conflict: bool, now: u64) -> bool {
        let mut changed = false;
        let mut ranked = false;
        let rank = (true, self.decided_seq + 1);
        if let Some(e) = self.entries.get_mut(id) {
            if matches!(e.phase, Phase::Waiting { answering: Some(_) }) {
                let outcome = match (conflict, e.wait.expired(now)) {
                    (true, _) => Some(Outcome::Closed),
                    (false, true) => Some(Outcome::Expired),
                    (false, false) => None,
                };
                e.phase = match outcome {
                    Some(outcome) => Phase::Decided {
                        outcome,
                        rank,
                        provisional: true,
                        answer_lost: false,
                    },
                    None => Phase::Waiting { answering: None },
                };
                changed = true;
                ranked = matches!(e.phase, Phase::Decided { .. });
            }
        }
        if ranked {
            self.decided_seq += 1;
        }
        if changed {
            self.trim_decided();
        }
        changed |= conflict && !std::mem::replace(&mut self.stale, true);
        changed
    }
}
