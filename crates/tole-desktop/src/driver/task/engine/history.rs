//! Reading the history: pages of `GET /entries` until the cursor reaches the
//! end, then the stream is opened from that cursor.
//!
//! The adapter's `history_page` says when to go on (`More`), when the history
//! is whole (`Complete`) and when a page cannot be trusted (`Stalled`). A page
//! that fails is asked again with `Backoff` (1 s doubling to 30 s) for ever;
//! pages that succeed follow each other at once (each is a round trip). A
//! history that stalls, or runs past `MAX_HISTORY_PAGES`, is left incomplete
//! (the status never says "nothing was changed") and the stream is opened from
//! the cursor all the same: it replays the rest.

use tole_client::{EntriesPage, Error};

use super::{millis, Effect, Engine, Now, Out};
use crate::adapter::approvals::Backoff;
use crate::adapter::task::{history_page, PageStep, MAX_HISTORY_PAGES};
use crate::driver::task::calls::{Call, Lane};

#[derive(Default)]
pub(super) struct History {
    /// Pages are still being read.
    reading: bool,
    /// The cursor: the last entry seen.
    after: u64,
    pages: usize,
    /// A page must be asked for when the pace allows.
    want: bool,
    retry_at: u64,
    backoff: Backoff,
}

impl History {
    pub(super) fn begin(&mut self) {
        self.reading = true;
        self.want = true;
    }

    pub(super) fn reading(&self) -> bool {
        self.reading
    }
}

impl Engine {
    /// Ask for the next page, when none is out and the retry wait is over.
    pub(super) fn history_next(&mut self, now: Now, out: &mut Out) -> bool {
        let h = &self.hist;
        if !(h.reading && h.want && now.mono >= h.retry_at && self.take_lane(Lane::Read)) {
            return false;
        }
        self.hist.want = false;
        let after = self.hist.after;
        out.effects
            .push(Effect::Call(Lane::Read, Call::Entries { after }));
        false
    }

    pub(super) fn history_reply(
        &mut self,
        after: u64,
        result: Result<EntriesPage, Error>,
        now: Now,
        out: &mut Out,
    ) -> bool {
        self.free_lane(Lane::Read);
        if !self.hist.reading || after != self.hist.after {
            return false;
        }
        let Ok(page) = result else {
            let wait = millis(self.hist.backoff.failed());
            self.hist.retry_at = now.mono.saturating_add(wait);
            self.hist.want = true;
            return false;
        };
        self.hist.backoff.succeeded();
        let read = history_page(&mut self.mapper, &page, after);
        let changed = self.apply(read.events);
        self.hist.pages = self.hist.pages.saturating_add(1);
        match read.step {
            PageStep::More { next_after } => {
                self.hist.after = next_after;
                if self.hist.pages < MAX_HISTORY_PAGES {
                    self.hist.want = true;
                } else {
                    self.finish_history(false, out);
                }
            }
            PageStep::Complete => {
                self.hist.after = page.next_after;
                self.finish_history(true, out);
            }
            PageStep::Stalled => self.finish_history(false, out),
        }
        changed
    }

    /// The history is in (`complete`) or cannot be read any further: open the
    /// stream from the cursor.
    pub(super) fn finish_history(&mut self, complete: bool, out: &mut Out) {
        self.hist.reading = false;
        {
            let mut shared = self.store.lock();
            shared.facts.loading = false;
            shared.task.history_complete = complete;
        }
        out.effects.push(Effect::OpenStream(self.hist.after));
        out.changed = true;
    }
}
