//! What the views read: the badge, the composer, the lists and the cards.

use super::super::card::{headline, ApprovalCard, CardState};
use super::super::request::Outcome;
use super::super::strings;
use super::super::summary::{bound, ActionSummary, MAX_DISPLAY_CHARS};
use super::{waiting, ApprovalsState, ComposerState, Entry, Phase, Row, Tab};
use crate::skills::Badge;

impl ApprovalsState {
    /// How many requests are listed as waiting. Not the badge: see
    /// [`ApprovalsState::waiting_badge`].
    pub fn waiting_count(&self) -> usize {
        self.entries.values().filter(|e| waiting(e)).count()
    }

    /// At least this many requests wait (the listed ones and those over the
    /// cap): the number to draw as "N+" while the badge is `Unknown`.
    pub fn waiting_at_least(&self) -> usize {
        self.waiting_count() + self.overflow.len()
    }

    /// The sidebar badge (`ShellState::approvals_pending`). A number only when
    /// the whole list is known and every request is counted: never loaded, a
    /// failed fetch, a lost connection, requests over the cap or refused rows
    /// all read `Unknown`, never a smaller number than the truth.
    pub fn waiting_badge(&self) -> Badge {
        let complete = self.known_all
            && self.overflow.is_empty()
            && self.saturated.is_none()
            && self.refusals.is_empty()
            && !self.entries.values().any(unreadable);
        if complete {
            Badge::Count(self.waiting_count())
        } else {
            Badge::Unknown
        }
    }

    /// Whether the message box of `session_id` works: disabled while a request
    /// of the task waits, and while the requests of the task are not known.
    pub fn composer(&self, session_id: &str) -> ComposerState {
        let mine = |s: Option<&str>| s == Some(session_id);
        let listed = (self.entries.values())
            .filter(|e| waiting(e) && mine(e.request.session_id.as_deref()))
            .count();
        let over = (self.overflow.values())
            .filter(|o| mine(o.session.as_deref()))
            .count();
        if listed + over > 0 {
            return ComposerState::WaitingForAnswer {
                requests: listed + over,
            };
        }
        let known = self.known_all || self.known_sessions.contains(session_id);
        let refused = (self.refusals.iter()).any(|r| r.any_session || mine(r.session.as_deref()));
        let unreadable =
            (self.entries.values()).any(|e| unreadable(e) && mine(e.request.session_id.as_deref()));
        if known && self.saturated.is_none() && !refused && !unreadable {
            ComposerState::Open
        } else {
            ComposerState::Checking
        }
    }

    /// The rows of a tab: waiting ones soonest first, decided ones newest first.
    pub fn rows(&self, tab: Tab, now: u64) -> Vec<Row> {
        let mut list: Vec<&Entry> = (self.entries.values())
            .filter(|e| waiting(e) == (tab == Tab::Waiting))
            .collect();
        list.sort_by(|a, b| match (&a.phase, &b.phase) {
            (Phase::Decided { rank: ra, .. }, Phase::Decided { rank: rb, .. }) => {
                rb.cmp(ra).then(a.seq.cmp(&b.seq)) // newest first
            }
            _ => (a.wait.deadline_ms().cmp(&b.wait.deadline_ms())).then(a.seq.cmp(&b.seq)),
        });
        list.into_iter().map(|e| self.row(e, now)).collect()
    }

    /// The card of a request, or of the selected one.
    pub fn card(&self, id: &str, now: u64) -> Option<ApprovalCard> {
        let e = self.entries.get(id)?;
        let mut card = ApprovalCard::build(&e.request, card_state(e), e.wait, now);
        if matches!(
            e.phase,
            Phase::Decided {
                answer_lost: true,
                ..
            }
        ) {
            card.state_note = Some(strings::ANSWER_LOST_NOTE);
        }
        Some(card)
    }

    pub fn selected_card(&self, now: u64) -> Option<ApprovalCard> {
        self.card(self.selected.as_deref()?, now)
    }

    /// A list row: worked out from the summary alone, without the preview and
    /// the details of a card (a list of 200 large requests stays cheap).
    fn row(&self, e: &Entry, now: u64) -> Row {
        let unknown;
        let summary = match &e.request.summary {
            Some(s) => s,
            None => {
                unknown = ActionSummary::unknown(&e.request.tool, None);
                &unknown
            }
        };
        let head = headline(&e.request, summary);
        let outcome = match e.phase {
            Phase::Decided { outcome, .. } => Some(outcome),
            Phase::Waiting { .. } => None,
        };
        let secs = e.wait.remaining_secs(now);
        Row {
            id: e.request.id.clone(),
            session_id: e.request.session_id.clone(),
            wording: head.wording,
            care: head.care,
            title: match head.outside_folder {
                true => format!(
                    "{}{}",
                    strings::list_title(head.wording),
                    strings::LIST_OUTSIDE_SUFFIX
                ),
                false => strings::list_title(head.wording).to_owned(),
            },
            outside_folder: head.outside_folder,
            subject: bound(&summary.target, MAX_DISPLAY_CHARS).0,
            left: outcome.is_none().then(|| strings::left_short(secs)),
            outcome,
            outcome_label: outcome.map(strings::outcome_label),
            selected: self.selected.as_deref() == Some(e.request.id.as_str()),
            from_helper: e.request.is_child(),
        }
    }

    /// The user picks a request of the shown tab; any other id is ignored.
    pub(super) fn select(&mut self, id: Option<String>) -> bool {
        let want_waiting = self.tab == Tab::Waiting;
        let next = match id {
            None => None,
            Some(id) => match self.entries.get(&id) {
                Some(e) if waiting(e) == want_waiting => Some(id),
                _ => return false,
            },
        };
        std::mem::replace(&mut self.selected, next.clone()) != next
    }

    /// The selection is cleared when its request no longer is on the shown
    /// tab (decided, expired, trimmed, or another tab): it never moves to
    /// another card by itself.
    pub(super) fn drop_selection(&mut self) -> bool {
        let want_waiting = self.tab == Tab::Waiting;
        let listed = (self.selected.as_deref())
            .and_then(|id| self.entries.get(id))
            .is_some_and(|e| waiting(e) == want_waiting);
        if self.selected.is_some() && !listed {
            self.selected = None;
            return true;
        }
        false
    }

    /// Selects a request when nothing is selected: the one that waits longest
    /// on the Waiting tab, the newest on the Decided tab. Called only when a
    /// request appears or the user changes tab.
    pub(super) fn auto_select(&mut self) -> bool {
        if self.selected.is_some() {
            return false;
        }
        let want_waiting = self.tab == Tab::Waiting;
        let tab = (self.entries.iter()).filter(|(_, e)| waiting(e) == want_waiting);
        let next = if want_waiting {
            tab.min_by_key(|(_, e)| (e.wait.deadline_ms(), e.seq))
        } else {
            tab.max_by_key(|(_, e)| match e.phase {
                Phase::Decided { rank, .. } => (rank, e.seq),
                Phase::Waiting { .. } => ((false, 0), e.seq),
            })
        };
        self.selected = next.map(|(id, _)| id.clone());
        self.selected.is_some()
    }
}

/// Decided only by a guess about a status this version cannot read: it may
/// be pending, so it is never counted as "nothing waits".
fn unreadable(e: &Entry) -> bool {
    matches!(
        e.phase,
        Phase::Decided {
            provisional: true,
            ..
        }
    ) && !Outcome::status_is_known(&e.request.status)
}

fn card_state(e: &Entry) -> CardState {
    match e.phase {
        Phase::Waiting { answering: None } => CardState::Waiting,
        Phase::Waiting { answering: Some(d) } => CardState::Answering(d),
        Phase::Decided { outcome, .. } => CardState::Decided(outcome),
    }
}
