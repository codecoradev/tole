//! History: pages of `GET /entries` to events, and when the history is whole.
//!
//! The driver loops: `after` = 0, call `Client::entries(id, after, limit)`, feed
//! the page here, apply the events, and on [`PageStep::More`] ask again with the
//! `next_after` it gives. On [`PageStep::Complete`] it sets
//! `TaskState::history_complete` (a new session has no history: set it at once).
//!
//! A page is checked against what serve's `page_of` guarantees: its entries are
//! above `after` and strictly increasing, `next_after` is the last entry's seq
//! (or `after` for an empty page), and the history is whole only when the cursor
//! has reached the page's own `last_seq`. A page that breaks this, a `has_more`
//! page with no entries, or a cursor that does not move would loop or end early
//! against a hostile or broken server: it is [`PageStep::Stalled`] and the
//! history is not complete (the status never says "Nothing was changed"). The
//! entries of such a page are still mapped (the model ignores old seqs).
//!
//! The driver also bounds the loop: at most [`MAX_HISTORY_PAGES`] pages.

use super::entry::EntryMapper;
use tole_client::EntriesPage;
use tole_desktop_model::task::TaskEvent;

/// Pages one history load may take before the driver gives up.
pub const MAX_HISTORY_PAGES: usize = 10_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PageStep {
    /// Ask again `after` this seq.
    More { next_after: u64 },
    /// The whole history was fed.
    Complete,
    /// More was promised but the cursor did not advance: stop, not complete.
    Stalled,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HistoryPage {
    pub events: Vec<TaskEvent>,
    pub step: PageStep,
}

/// Map one page fetched with `after`.
pub fn history_page(mapper: &mut EntryMapper, page: &EntriesPage, after: u64) -> HistoryPage {
    let events = page.entries.iter().flat_map(|e| mapper.entry(e)).collect();
    let seqs = page.entries.iter().map(|e| e.seq);
    let ordered = seqs.clone().zip(seqs.skip(1)).all(|(a, b)| a < b);
    let above = page.entries.first().is_none_or(|e| e.seq > after);
    let last = page.entries.last().map_or(after, |e| e.seq);
    let step = if !(ordered && above && page.next_after == last) {
        PageStep::Stalled
    } else if page.has_more {
        if page.entries.is_empty() {
            PageStep::Stalled
        } else {
            PageStep::More { next_after: last }
        }
    } else if page.next_after >= page.last_seq {
        PageStep::Complete
    } else if page.entries.is_empty() {
        PageStep::Stalled
    } else {
        // Entries were appended since this page was cut: read on.
        PageStep::More { next_after: last }
    };
    HistoryPage { events, step }
}
