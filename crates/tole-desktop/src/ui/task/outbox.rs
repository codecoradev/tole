//! A message on its way: when the words must come back into the composer.
//!
//! The driver takes a prompt or a note as a queued command (`Ok` means queued,
//! not accepted) and answers in its next views: a command it refuses raises
//! `TaskView::ignored`, and a send that failed puts the words in
//! `TaskView::draft`. The window clears the box when it sends, so what comes
//! back is the window's to give back, merged with what was typed since (the
//! model's `Composer::restore`). No clock: a send is settled by what the views
//! say, and only by that:
//!
//! - the transcript shows one more of the owner's rows (`owner_rows`): the
//!   driver took it, and nothing later can give it back (a refused Stop, a
//!   second click on an approval, another message);
//! - `ignored` rises first: the driver refused it, and the words come back.
//!
//! A change of what Enter does (a steer note while a turn runs, a prompt when
//! it ended) settles nothing: a turn that ends can end between the note and its
//! refusal, which shows one look later.

use tole_desktop_model::task::{Composer, SendMode, Submission};

/// What the last views said, as far as a send cares.
#[derive(Debug, Clone, Copy)]
pub struct Seen<'a> {
    /// `TaskView::ignored`.
    pub ignored: u64,
    /// `TaskView::draft`.
    pub draft: &'a str,
    /// `TaskView::owner_rows`.
    pub owner_rows: usize,
}

#[derive(Debug)]
struct Pending {
    sent: Submission,
    ignored: u64,
    owner_rows: usize,
}

#[derive(Debug, Default)]
pub struct Outbox {
    pending: Option<Pending>,
    /// The draft already given back.
    draft_seen: String,
}

impl Outbox {
    /// `sent` was queued while the views said `seen`.
    pub fn sent(&mut self, sent: Submission, seen: &Seen<'_>) {
        self.draft_seen.clear();
        self.pending = Some(Pending {
            sent,
            ignored: seen.ignored,
            owner_rows: seen.owner_rows,
        });
    }

    /// The text the box must hold now, or `None` to leave it. `typed` is what
    /// the box holds.
    pub fn back(&mut self, seen: &Seen<'_>, typed: &str) -> Option<String> {
        let mut composer = Composer::with_text(typed);
        let mut restored = false;
        // A refused command adds no row, so a new row is the message taken,
        // whatever else was refused since.
        if self
            .pending
            .take_if(|p| seen.owner_rows > p.owner_rows)
            .is_some()
        {
            // taken: settled
        } else if let Some(pending) = self.pending.take_if(|p| seen.ignored > p.ignored) {
            composer.restore(pending.sent);
            restored = true;
        }
        if !seen.draft.is_empty() && seen.draft != self.draft_seen {
            self.draft_seen = seen.draft.to_owned();
            let words = Submission {
                mode: SendMode::Send,
                text: seen.draft.to_owned(),
                attachments: Vec::new(),
            };
            composer.restore(words);
            restored = true;
        }
        restored.then_some(composer.text)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seen(ignored: u64, draft: &str, owner_rows: usize) -> Seen<'_> {
        Seen {
            ignored,
            draft,
            owner_rows,
        }
    }

    fn words(text: &str) -> Submission {
        Submission {
            mode: SendMode::Send,
            text: text.into(),
            attachments: vec![],
        }
    }

    #[test]
    fn a_command_the_driver_refuses_brings_the_words_back_before_what_was_typed_since() {
        let mut outbox = Outbox::default();
        outbox.sent(words("tidy up"), &seen(4, "", 3));
        assert_eq!(outbox.back(&seen(4, "", 3), ""), None, "not answered yet");
        assert_eq!(
            outbox.back(&seen(5, "", 3), "and also").as_deref(),
            Some("tidy up\nand also")
        );
        assert_eq!(outbox.back(&seen(6, "", 3), "x"), None, "settled once");
    }

    #[test]
    fn a_note_the_driver_took_is_settled_by_its_row_and_a_later_refusal_is_not_ours() {
        let mut outbox = Outbox::default();
        outbox.sent(words("use folder B"), &seen(2, "", 3));
        // The turn goes on and the note is not shown yet: still on its way.
        assert_eq!(outbox.back(&seen(2, "", 3), ""), None);
        // The transcript shows it: taken.
        assert_eq!(outbox.back(&seen(2, "", 4), ""), None);
        // A double-click on Stop is refused: the note must not come back.
        assert_eq!(outbox.back(&seen(3, "", 4), ""), None);
        assert_eq!(outbox.back(&seen(4, "", 4), ""), None);
    }

    #[test]
    fn a_note_refused_as_the_turn_ends_comes_back_one_look_after_the_mode_changed() {
        let mut outbox = Outbox::default();
        outbox.sent(words("use folder B"), &seen(0, "", 3));
        // Look 1: the turn ended, so Enter is a prompt now, and the refusal is not shown yet.
        assert_eq!(outbox.back(&seen(0, "", 3), ""), None);
        // Look 2: the driver's refusal.
        assert_eq!(
            outbox.back(&seen(1, "", 3), "").as_deref(),
            Some("use folder B")
        );
    }

    #[test]
    fn a_refusal_and_a_new_row_in_one_look_are_the_message_taken() {
        let mut outbox = Outbox::default();
        outbox.sent(words("use folder B"), &seen(0, "", 3));
        assert_eq!(
            outbox.back(&seen(1, "", 4), ""),
            None,
            "a refused send adds no row"
        );
    }

    #[test]
    fn a_failed_send_comes_back_once_through_the_draft_and_again_for_a_new_failure() {
        let mut outbox = Outbox::default();
        let failed = seen(0, "tidy up", 3);
        assert_eq!(outbox.back(&failed, "").as_deref(), Some("tidy up"));
        assert_eq!(
            outbox.back(&failed, ""),
            None,
            "the owner may have cleared it"
        );
        outbox.sent(words("tidy up"), &seen(0, "", 3));
        assert_eq!(
            outbox.back(&failed, "").as_deref(),
            Some("tidy up"),
            "a new failure"
        );
    }

    #[test]
    fn a_refusal_and_a_draft_together_give_the_words_back_in_order() {
        let mut outbox = Outbox::default();
        outbox.sent(words("second"), &seen(0, "", 3));
        let both = seen(1, "first", 3);
        assert_eq!(outbox.back(&both, "").as_deref(), Some("first\nsecond"));
    }
}
