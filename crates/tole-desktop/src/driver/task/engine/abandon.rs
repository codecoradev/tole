//! Giving up a prompt call that never answers.
//!
//! The prompt call answers when its turn ends. After a suspend on a remote
//! serve its socket can die without a word; the stream then reconnects and
//! delivers the answer, the model reads idle, and the call would hold the
//! prompt lane (and the send button) for as long as the client's read timeout.
//! So when the model has been idle for [`ABANDON_AFTER`] with the call still
//! out (a few seconds after a suspend), the call is abandoned: its generation
//! is retired, so its late reply is dropped without touching the model, the
//! lane is free, and the worker starts another thread for it (`ReplaceLane`).
//! The old thread ends when its call returns and is counted as detached at
//! shutdown if it has not. The words of the call come back to the draft unless
//! the transcript already shows them (`give_back`).

use tole_desktop_model::task::{Item, SendMode, Submission};

use super::{Effect, Engine, Now, Out};
use crate::driver::task::calls::Lane;

/// How long the model may read idle while the prompt call is still out. The
/// reply of a call that works follows the end of the turn within milliseconds.
const ABANDON_AFTER_MS: u64 = 30_000;
/// The same after a suspend, when the call is the likeliest thing to be dead.
const ABANDON_AFTER_SUSPEND_MS: u64 = 3_000;

impl Engine {
    /// The words of an abandoned call, or of a failed one that may have
    /// reached the serve: when no row of the transcript is the owner's message
    /// with that text, they go back to the draft and the view says the message
    /// may not have been sent (the old request cannot be cancelled: it may
    /// still reach the serve).
    pub(super) fn give_back(&mut self, sub: Submission) {
        let mut shared = self.store.lock();
        let steer = sub.mode == SendMode::Steer;
        let shown = (shared.task.transcript.items.iter().rev().take(32)).any(
            |row| matches!(row, Item::User { text, steer: s, .. } if *s == steer && *text == sub.text),
        );
        if !shown {
            shared.task.send_failed(sub, false);
            shared.facts.unsent = true;
        }
    }

    pub(super) fn prompt_abandon(&mut self, now: Now, out: &mut Out) -> bool {
        let busy = self.store.lock().task.run.busy;
        let bound = if self.sending.jumped {
            ABANDON_AFTER_SUSPEND_MS
        } else {
            ABANDON_AFTER_MS
        };
        let Some(slot) = &mut self.sending.prompt else {
            return false;
        };
        // A call not started yet is taken back by Stop, not abandoned.
        if busy || !slot.gate.started() {
            slot.idle_since = None;
            return false;
        }
        let since = *slot.idle_since.get_or_insert(now.mono);
        if now.mono.saturating_sub(since) < bound {
            return false;
        }
        let sent = self.sending.prompt.take().and_then(|slot| slot.sub);
        if let Some(sub) = sent {
            self.give_back(sub);
        }
        self.sending.generation = self.sending.generation.wrapping_add(1);
        self.sending.jumped = false;
        self.free_lane(Lane::Prompt);
        self.store.lock().facts.prompting = false;
        out.effects.push(Effect::ReplaceLane(Lane::Prompt));
        true
    }
}
