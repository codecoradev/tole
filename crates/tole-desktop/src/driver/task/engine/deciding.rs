//! The owner's answer to an approval of this task: one decision in flight.
//!
//! The model of a task keeps its approvals as plain cards, so the guards are
//! here: only an id the model shows is accepted, once, and only when the
//! serve's own id of it is certain (`ids`); the answer goes under that raw id.
//! Before a queued answer leaves it is checked against the model again (the
//! card may have been answered on another face while it waited). What comes
//! back is applied as the serve's word only when it names this request and says
//! how it ended (`ApprovalResolved`, an allow resumes the turn: `watching`
//! polls it); any other result, a failure included, means the answer may or
//! may not have landed, and the queue is read again. A Stop on cards that
//! cannot be answered cancels the turn and drops them (`drop_undecidable`).

use std::collections::VecDeque;

use tole_client::{DecisionReply, Error};
use tole_desktop_model::approvals::{Decision, Outcome};
use tole_desktop_model::task::TaskEvent;

use super::{Effect, Engine, Now, Out};
use crate::driver::task::calls::{Call, Lane};

#[derive(Default)]
pub(super) struct Deciding {
    /// The cards shown (cleaned ids), the serve's id of each when accepted, and
    /// what the owner chose.
    queue: VecDeque<(String, String, Decision)>,
    /// The card being answered.
    flight: Option<String>,
}

impl Engine {
    pub(super) fn decide(&mut self, id: &str, decision: Decision) -> bool {
        let known = (self.store.lock().task.run.approvals.iter()).any(|a| a.id == id);
        let raw = self.ids.borrow().raw(id).map(str::to_owned);
        let d = &mut self.deciding;
        let twice = d.flight.as_deref() == Some(id) || d.queue.iter().any(|(q, ..)| q == id);
        let (true, false, Some(raw)) = (known, twice, raw) else {
            return false;
        };
        d.queue.push_back((id.to_owned(), raw, decision));
        self.sync_deciding();
        true
    }

    /// The next queued answer, if the lane is free and the model still shows
    /// the card.
    pub(super) fn decide_next(&mut self, out: &mut Out) -> bool {
        let mut changed = false;
        while !self.lane_busy(Lane::Decide) {
            let Some((shown, accepted, decision)) = self.deciding.queue.pop_front() else {
                break;
            };
            // The same card, and still the same request: the serve's id of it
            // is what it was when the owner chose (a card replaced meanwhile
            // by another request of the same look is not answered).
            let live = (self.store.lock().task.run.approvals.iter()).any(|a| a.id == shown);
            let same = self.ids.borrow().raw(&shown) == Some(accepted.as_str());
            if live && same {
                self.take_lane(Lane::Decide);
                self.deciding.flight = Some(shown);
                let id = accepted;
                out.effects
                    .push(Effect::Call(Lane::Decide, Call::Decide { id, decision }));
            } else if live {
                self.store.lock().facts.ignored += 1;
            }
            changed = true;
        }
        if changed {
            self.sync_deciding();
        }
        changed
    }

    /// `raw` is the id the call was made with.
    pub(super) fn decide_reply(
        &mut self,
        raw: &str,
        result: Result<DecisionReply, Error>,
        now: Now,
    ) -> bool {
        self.free_lane(Lane::Decide);
        self.deciding.flight = None;
        self.sync_deciding();
        let verdict = result
            .ok()
            .filter(|r| r.id == raw)
            .and_then(|r| Outcome::of_status(&r.status).map(|o| (o, r.status)));
        match verdict {
            Some((_, status)) => self.apply(vec![TaskEvent::ApprovalResolved {
                id: raw.to_owned(),
                approved: status == "approved",
            }]),
            None => {
                self.refresh.want(now);
                false
            }
        }
    }

    /// A Stop was accepted for cards nobody can answer from here: they are
    /// shown as denied, and the queue is read again (the model keeps what it
    /// was told is decided until the next reconnect, so a request the serve
    /// still holds shows again then).
    pub(super) fn drop_undecidable(&mut self, now: Now) {
        let cards = self.store.lock().facts.undecidable.clone();
        let events = cards.into_iter().map(|id| TaskEvent::ApprovalResolved {
            id,
            approved: false,
        });
        self.apply(events.collect());
        self.refresh.want(now);
    }

    /// What the view says is being answered.
    fn sync_deciding(&self) {
        let d = &self.deciding;
        let ids = d.flight.iter().cloned();
        let ids = ids
            .chain(d.queue.iter().map(|(id, ..)| id.clone()))
            .collect();
        self.store.lock().facts.deciding = ids;
    }
}
