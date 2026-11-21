//! What the views read: the badge, the composer, the lists and the cards.

use super::super::card::{ApprovalCard, CardState};
use super::super::strings;
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
            && !self.overflow_saturated
            && self.refused == 0;
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
        if known && !self.overflow_saturated {
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
        list.sort_by_key(|e| match e.phase {
            Phase::Decided { at_ms, .. } => (-i128::from(at_ms), e.seq),
            Phase::Waiting { .. } => (i128::from(e.wait.deadline_ms()), e.seq),
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

    fn row(&self, e: &Entry, now: u64) -> Row {
        let card = ApprovalCard::build(&e.request, card_state(e), e.wait, now);
        let outcome = match e.phase {
            Phase::Decided { outcome, .. } => Some(outcome),
            Phase::Waiting { .. } => None,
        };
        Row {
            id: e.request.id.clone(),
            session_id: e.request.session_id.clone(),
            wording: card.wording,
            care: card.care,
            title: match card.outside_folder {
                true => format!(
                    "{}{}",
                    strings::list_title(card.wording),
                    strings::LIST_OUTSIDE_SUFFIX
                ),
                false => strings::list_title(card.wording).to_owned(),
            },
            outside_folder: card.outside_folder,
            subject: card.subject,
            left: card.left_short.filter(|_| outcome.is_none()),
            outcome,
            outcome_label: outcome.map(strings::outcome_label),
            selected: self.selected.as_deref() == Some(e.request.id.as_str()),
            from_helper: e.request.is_child(),
        }
    }

    /// Keeps the selection on a request the shown tab lists (see the module
    /// doc); `true` when it moved.
    pub(super) fn follow_selection(&mut self) -> bool {
        let want_waiting = self.tab == Tab::Waiting;
        let listed = (self.selected.as_deref())
            .and_then(|id| self.entries.get(id))
            .is_some_and(|e| waiting(e) == want_waiting);
        if listed {
            return false;
        }
        let tab = (self.entries.iter()).filter(|(_, e)| waiting(e) == want_waiting);
        let next = if want_waiting {
            tab.min_by_key(|(_, e)| (e.wait.deadline_ms(), e.seq))
        } else {
            tab.max_by_key(|(_, e)| match e.phase {
                Phase::Decided { at_ms, .. } => (at_ms, e.seq),
                Phase::Waiting { .. } => (0, e.seq),
            })
        };
        let next = next.map(|(id, _)| id.clone());
        let moved = next != self.selected;
        self.selected = next;
        moved
    }
}

fn card_state(e: &Entry) -> CardState {
    match e.phase {
        Phase::Waiting { answering: None } => CardState::Waiting,
        Phase::Waiting { answering: Some(d) } => CardState::Answering(d),
        Phase::Decided { outcome, .. } => CardState::Decided(outcome),
    }
}
