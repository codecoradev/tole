//! The owner's three actions on a running task: a prompt, a steer note and
//! Stop. Each goes through the model first (`press_enter`, `press_stop`), which
//! decides whether it is allowed at all, and then through one call.
//!
//! - **Prompt.** The call blocks until the turn stops, so it has its own lane,
//!   and a second prompt waits until it returned (the view says so): a late
//!   `TurnEnded` of the first would end the second turn. Its reply is the
//!   adapter's `prompt_outcome`. A failure gives the words back
//!   (`send_failed`) and, when the request may have reached the serve, asks
//!   the status once whether the turn runs anyway.
//! - **Stop during the send.** Until the lane thread has started the call
//!   (`Gate`) the send can still be taken back: Stop then makes no call, and
//!   the model gets `send_failed(sent, false)`, as its docs require. After
//!   that Stop is a cancel call (`stop_step`), and when the serve answers that
//!   it is cancelling, `BusyPolling::after_stop` watches until the turn ends
//!   or Stop is offered again.
//! - **An abandoned call.** A prompt call that never answers (the socket died
//!   silently in a suspend) must not hold the lane for hours while the stream
//!   says the turn is over: see `abandon`. Its reply carries the generation of
//!   the send it belongs to, and one of an older generation is dropped.
//! - **A call that returned.** The model is told so (`Run::prompt_returned`):
//!   a reply with no row (a bare `approval_required`) whose user entry the
//!   stream never delivered would otherwise keep the turn busy for ever.
//! - **A resumed turn.** A prompt reply that arrives while an allowed turn is
//!   running belongs to the part of the turn that asked: its `TurnEnded` is
//!   dropped, it must not end the resumed one.

use tole_client::PromptRequest;
use tole_client::{CancelReply, Error, InterruptReply, InterruptRequest, PromptReply};
use tole_desktop_model::task::{Enter, EnterOutcome, Phase, SendMode, Submission, TaskEvent};

use super::watching::Mode;
use super::{Effect, Engine, Now, Out};
use crate::adapter::task::{
    may_have_been_received, prompt_outcome, stop_step, PromptOutcome, StopStep,
};
use crate::driver::task::calls::{Call, Gate, Lane};

const ENTER: Enter = Enter {
    shift: false,
    composing: false,
};

/// The send that is out.
pub(super) struct Slot {
    /// `None` once the model has it back (a taken-back send).
    pub(super) sub: Option<Submission>,
    pub(super) gate: Gate,
    /// When the model was first seen idle while this call was still out.
    pub(super) idle_since: Option<u64>,
}

#[derive(Default)]
pub(super) struct Sending {
    pub(super) prompt: Option<Slot>,
    steer: Option<Submission>,
    /// The generation of the latest prompt call.
    pub(super) generation: u64,
    /// A suspend was seen while the call was out.
    pub(super) jumped: bool,
    /// The Stop call out is for cards that cannot be answered, not a turn.
    cards_cancel: bool,
}

impl Engine {
    /// Submit through the model's composer; `None` when the model would not
    /// send (wrong mode, blank text) or the driver cannot take it yet.
    fn submit(&mut self, text: String, mode: SendMode, lane: Lane) -> Option<Submission> {
        let blank = text.trim().is_empty();
        if self.hist.reading() || !self.link.live || self.lane_busy(lane) || blank {
            return None;
        }
        let mut shared = self.store.lock();
        if shared.task.status().send != mode {
            return None;
        }
        shared.task.composer.text = text;
        match shared.task.press_enter(ENTER) {
            EnterOutcome::Submit(sub) if sub.mode == mode => Some(sub),
            _ => None,
        }
    }

    pub(super) fn send(&mut self, text: String, out: &mut Out) -> bool {
        let Some(sub) = self.submit(text, SendMode::Send, Lane::Prompt) else {
            return false;
        };
        // Images are not attached by this driver: the composer holds none.
        let req = PromptRequest {
            text: sub.text.clone(),
            images: Vec::new(),
        };
        // A status check of an earlier failed send says nothing of this turn.
        self.watch.cancel_check();
        let gate = Gate::default();
        let generation = self.sending.generation.wrapping_add(1);
        self.sending.generation = generation;
        self.sending.jumped = false;
        self.store.lock().facts.unsent = false;
        self.sending.prompt = Some(Slot {
            sub: Some(sub),
            gate: gate.clone(),
            idle_since: None,
        });
        self.take_lane(Lane::Prompt);
        self.store.lock().facts.prompting = true;
        out.effects.push(Effect::Call(
            Lane::Prompt,
            Call::Prompt {
                req,
                gate,
                generation,
            },
        ));
        true
    }

    pub(super) fn steer(&mut self, text: String, out: &mut Out) -> bool {
        let Some(sub) = self.submit(text, SendMode::Steer, Lane::Steer) else {
            return false;
        };
        let req = InterruptRequest::text(sub.text.clone());
        self.sending.steer = Some(sub);
        self.take_lane(Lane::Steer);
        self.store.lock().facts.steering = true;
        out.effects
            .push(Effect::Call(Lane::Steer, Call::Interrupt(req)));
        true
    }

    pub(super) fn stop(&mut self, out: &mut Out) -> bool {
        if !self.store.lock().task.press_stop() {
            return self.stop_cards(out);
        }
        if let Some(slot) = &mut self.sending.prompt {
            if slot.gate.abort() {
                // Not made yet: nothing to cancel, the words come back.
                if let Some(sub) = slot.sub.take() {
                    self.store.lock().task.send_failed(sub, false);
                }
                return true;
            }
        }
        if self.take_lane(Lane::Stop) {
            out.effects.push(Effect::Call(Lane::Stop, Call::Cancel));
        } else {
            // A cancel is still out: offer Stop again rather than stack another.
            self.store.lock().task.stop_failed();
        }
        true
    }

    /// Stop on a task that waits for an OK it cannot be given (the card
    /// cannot be answered from here): the model offers no Stop there, so the
    /// driver cancels the turn itself and, once the serve says it did, drops
    /// those cards (`cancel_reply`); a queue read brings back any that stay.
    fn stop_cards(&mut self, out: &mut Out) -> bool {
        let waiting = {
            let shared = self.store.lock();
            !shared.facts.undecidable.is_empty()
                && shared.task.status().phase == Phase::WaitingApproval
        };
        if !waiting || !self.take_lane(Lane::Stop) {
            return false;
        }
        self.sending.cards_cancel = true;
        out.effects.push(Effect::Call(Lane::Stop, Call::Cancel));
        true
    }

    pub(super) fn prompt_reply(
        &mut self,
        generation: u64,
        result: Option<Result<PromptReply, Error>>,
        now: Now,
    ) -> bool {
        if generation != self.sending.generation {
            return false; // an abandoned call, answering late: nothing is its own
        }
        self.free_lane(Lane::Prompt);
        self.store.lock().facts.prompting = false;
        let sub = self.sending.prompt.take().and_then(|slot| slot.sub);
        let Some(result) = result else {
            return true; // taken back; the model already has its words
        };
        match prompt_outcome(&result) {
            PromptOutcome::Ended { events, .. } => {
                // The status watcher (`watching`) takes over whenever the
                // model is still busy: that is what `check_status` asks for.
                let resumed = {
                    let shared = self.store.lock();
                    shared.task.run.unobserved && shared.task.run.busy
                };
                let keep = |e: &TaskEvent| !(resumed && matches!(e, TaskEvent::TurnEnded));
                self.apply(events.into_iter().filter(keep).collect());
                self.store.lock().task.run.prompt_returned();
            }
            PromptOutcome::Failed {
                server_busy,
                check_status,
            } => {
                if let Some(sub) = sub {
                    // A failure that may have come after the request reached
                    // the serve: the turn can run, and the stream may already
                    // show the words, so they are not put back blindly.
                    if check_status && !server_busy {
                        self.give_back(sub);
                    } else {
                        self.store.lock().task.send_failed(sub, server_busy);
                    }
                }
                if check_status && !server_busy {
                    self.watch.check_soon(now);
                }
            }
        }
        true
    }

    pub(super) fn steer_reply(&mut self, result: Result<InterruptReply, Error>, now: Now) -> bool {
        self.free_lane(Lane::Steer);
        self.store.lock().facts.steering = false;
        let (Err(error), Some(sub)) = (result, self.sending.steer.take()) else {
            return true;
        };
        if may_have_been_received(&error) {
            self.give_back(sub);
        } else {
            self.store.lock().task.send_failed(sub, false);
        }
        // A 409 says no turn runs: the model may be wrong that one does.
        if matches!(error, Error::Status { status: 409, .. }) {
            self.watch.check_soon(now);
        }
        true
    }

    pub(super) fn cancel_reply(&mut self, result: Result<CancelReply, Error>, now: Now) -> bool {
        self.free_lane(Lane::Stop);
        if std::mem::take(&mut self.sending.cards_cancel) {
            if result.is_ok() {
                self.drop_undecidable(now);
            }
            return true;
        }
        match stop_step(&result) {
            StopStep::Wait => self.watch.start(Mode::after_stop(), now),
            other => other.apply(&mut self.store.lock().task),
        }
        true
    }
}
