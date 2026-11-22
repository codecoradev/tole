//! The owner's answers: what is queued, what is sent, what a reply does.

use tole_client::{DecisionReply, Error};
use tole_desktop_model::approvals::{
    AnswerError, ApprovalsAction, CardState, Decision, DecisionIntent, Outcome,
};

use super::{Effect, Engine, Now, Out};
use crate::adapter::approvals::{resolved_from_reply, status_of};

impl Engine {
    /// The owner pressed a button. The model accepts it only for a request that
    /// waits and is not being answered, so there is never more than one queued
    /// intent for an id.
    pub(super) fn decide(&mut self, id: &str, decision: Decision, now: Now) -> bool {
        let answer = self.store.lock().answer(id, decision, now.wall);
        match answer {
            Ok(intent) => {
                self.decide_queue.push_back(intent);
                true
            }
            // Nothing changed: not a request, or one already answered.
            Err(AnswerError::Unknown | AnswerError::NotWaiting) => false,
            // Its time was up: the model moved it to Decided.
            Err(AnswerError::Expired) => true,
        }
    }

    /// The next queued decision, if the call is free and the model still says
    /// the request is being answered with that very decision. A request that
    /// expired, was answered by someone else or was answered again by the
    /// owner while the intent waited is not sent: the model already shows what
    /// became of it.
    ///
    /// The model hears the wall time first: a card's expiry reaches its phase
    /// only through `Tick`, so without it an allow could leave a moment after
    /// its time ran out. Returns whether the model changed.
    pub(super) fn dispatch_decisions(&mut self, now: Now, out: &mut Out) -> bool {
        let mut changed = false;
        while !self.decide_busy {
            let Some(intent) = self.decide_queue.pop_front() else {
                return changed;
            };
            changed |= self.store.lock().apply(ApprovalsAction::Tick(now.wall));
            let card = self.store.lock().card(&intent.id, now.wall);
            let live = card.is_some_and(|c| c.state == CardState::Answering(intent.decision));
            if live {
                self.decide_busy = true;
                out.effects.push(Effect::Send(intent));
            }
        }
        changed
    }

    /// The decision call ended. The server's word is applied only when it
    /// names the request that was answered and says how it ended; a reply of
    /// another id (a hostile or broken server) must not add a row of its own.
    pub(super) fn decided(
        &mut self,
        intent: DecisionIntent,
        reply: Result<DecisionReply, Error>,
        now: Now,
    ) -> bool {
        self.decide_busy = false;
        let id = intent.id;
        let (action, unsure) = match reply {
            Ok(r) if r.id == id && Outcome::of_status(&status_of(&r.id, &r.status)).is_some() => (
                ApprovalsAction::Resolved(resolved_from_reply(&r), now.wall),
                false,
            ),
            // Not a decision, or not about this request: the answer did not
            // land as far as this view can tell, and the buttons come back.
            Ok(_) => (ApprovalsAction::AnswerFailed(id, false, now.wall), true),
            Err(Error::Status { status: 409, .. }) => {
                (ApprovalsAction::AnswerFailed(id, true, now.wall), false)
            }
            // The call may have reached the server: look again soon.
            Err(_) => (ApprovalsAction::AnswerFailed(id, false, now.wall), true),
        };
        if unsure {
            self.refresh_asap = true;
        }
        self.apply(action, now)
    }
}
