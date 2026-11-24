//! What the Approvals page says about its list (board S5), as plain data.
//!
//! The queue knows what it knows ([`super::ApprovalsState`]); this module only
//! words it for the page: the top bar, the tabs, an empty list, the notices
//! about what could not be read, and the order Tab visits the controls in. An
//! unknown list never reads as an empty one: "Nothing is waiting" is said only
//! for a list the queue knows.

use super::card::{ApprovalCard, Button, BUTTONS};
use super::queue::{Row, Tab};
use super::strings as s;
use crate::skills::Badge;

/// What the page knows about the list it shows: the fields of the driver's
/// view that bear on whether the list can be trusted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Standing {
    /// The sidebar badge: a number only when the whole list is known.
    pub badge: Badge,
    /// At least this many requests wait.
    pub waiting_at_least: usize,
    /// Rows the server sent that could not be read.
    pub refused: usize,
    /// Fetches of the whole list that failed in a row.
    pub fetch_failures: u32,
    /// Tasks followed only by the periodic refresh.
    pub parked: usize,
}

impl Standing {
    /// Whether the whole list is known now.
    pub fn known(&self) -> bool {
        matches!(self.badge, Badge::Count(_))
    }

    /// "1 waiting", "Nothing is waiting", or, when the list is not known,
    /// what is certain: never a zero.
    pub fn waiting_summary(&self) -> String {
        match self.badge {
            Badge::Count(0) => s::NOTHING_WAITING.to_owned(),
            Badge::Count(n) => s::waiting_count(n),
            Badge::Unknown if self.waiting_at_least > 0 => {
                s::at_least_waiting(self.waiting_at_least)
            }
            Badge::Unknown => s::CHECKING_WAITING.to_owned(),
        }
    }

    /// The line at the right of the top bar.
    pub fn top_line(&self) -> String {
        s::top_line(&self.waiting_summary())
    }

    /// The label of a tab. The Waiting tab carries the count, or the unknown
    /// mark while the count is not known.
    pub fn tab_label(&self, tab: Tab) -> String {
        match (tab, self.badge) {
            (Tab::Decided, _) | (Tab::Waiting, Badge::Count(0)) => tab_name(tab).to_owned(),
            (Tab::Waiting, Badge::Count(n)) => s::tab_with_count(tab_name(tab), &n.to_string()),
            (Tab::Waiting, Badge::Unknown) => {
                s::tab_with_count(tab_name(tab), crate::strings::BADGE_UNKNOWN)
            }
        }
    }

    /// What an empty tab says. It says "nothing" only for a known list; for an
    /// unknown one it says it is checking, or that it cannot reach the server.
    pub fn empty_text(&self, tab: Tab) -> &'static str {
        match (self.known(), tab, self.fetch_failures) {
            (true, Tab::Waiting, _) => s::EMPTY_WAITING,
            (true, Tab::Decided, _) => s::EMPTY_DECIDED,
            (false, _, 0) => s::CHECKING,
            (false, _, _) => s::UNREACHABLE,
        }
    }

    /// Lines about what this list cannot be trusted for, above the list. An
    /// unknown list that shows rows says it may be incomplete; a row that
    /// could not be read says a request may be waiting.
    pub fn notices(&self, rows_shown: usize) -> Vec<String> {
        let mut out = Vec::new();
        if !self.known() && rows_shown > 0 {
            out.push(s::INCOMPLETE_NOTICE.to_owned());
        }
        if self.refused > 0 {
            out.push(s::refused_notice(self.refused));
        }
        if self.parked > 0 {
            out.push(s::PARKED_NOTICE.to_owned());
        }
        out
    }
}

fn tab_name(tab: Tab) -> &'static str {
    match tab {
        Tab::Waiting => s::TAB_WAITING,
        Tab::Decided => s::TAB_DECIDED,
    }
}

/// A control of the page that Tab visits.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum PageStop {
    TabWaiting,
    TabDecided,
    /// A request of the list, by id.
    Row(String),
    /// "Show technical details" of the card.
    Details,
    /// "Don't allow" of the card.
    DontAllow,
    /// "Allow once" of the card.
    AllowOnce,
}

impl PageStop {
    /// The stop of a button of the card.
    pub fn of_button(button: Button) -> PageStop {
        match button {
            Button::DontAllow => PageStop::DontAllow,
            Button::AllowOnce => PageStop::AllowOnce,
        }
    }
}

/// The controls Tab visits, first to last: the two tabs, the rows of the shown
/// tab, then the card's own controls: the buttons, then the technical details.
/// The buttons are stops only while the card can be answered, so a key never
/// reaches a button that does nothing, and the last stop is never an answer.
pub fn page_stops(rows: &[Row], selected: Option<&ApprovalCard>) -> Vec<PageStop> {
    let mut stops = vec![PageStop::TabWaiting, PageStop::TabDecided];
    stops.extend(rows.iter().map(|r| PageStop::Row(r.id.clone())));
    if let Some(card) = selected {
        if card.answerable() {
            stops.extend(BUTTONS.iter().map(|b| PageStop::of_button(*b)));
        }
        // Last on purpose: a key that starts from nothing and goes backwards
        // (Shift+Tab) lands on the last stop, and that must never be an answer.
        stops.push(PageStop::Details);
    }
    stops
}

#[cfg(test)]
mod tests {
    use super::super::fixtures::{req, T0};
    use super::super::{ApprovalsAction as A, ApprovalsState, FetchId, Scope, Tab};
    use super::*;

    fn standing(badge: Badge) -> Standing {
        Standing {
            badge,
            waiting_at_least: 0,
            refused: 0,
            fetch_failures: 0,
            parked: 0,
        }
    }

    #[test]
    fn an_unknown_list_never_reads_as_zero_or_as_empty() {
        let unknown = standing(Badge::Unknown);
        assert_eq!(unknown.waiting_summary(), s::CHECKING_WAITING);
        assert_eq!(unknown.empty_text(Tab::Waiting), s::CHECKING);
        assert_eq!(unknown.empty_text(Tab::Decided), s::CHECKING);
        let failing = Standing {
            fetch_failures: 3,
            ..unknown
        };
        assert_eq!(failing.empty_text(Tab::Waiting), s::UNREACHABLE);
        for text in [unknown.top_line(), failing.top_line()] {
            assert!(!text.contains("Nothing") && !text.contains(" 0 "), "{text}");
        }
        // What is listed is certain even when the rest is not.
        let some = Standing {
            waiting_at_least: 2,
            ..unknown
        };
        assert_eq!(some.waiting_summary(), "At least 2 waiting");
    }

    #[test]
    fn a_known_list_says_nothing_only_when_it_is_empty() {
        assert_eq!(
            standing(Badge::Count(0)).waiting_summary(),
            s::NOTHING_WAITING
        );
        assert_eq!(
            standing(Badge::Count(0)).empty_text(Tab::Waiting),
            s::EMPTY_WAITING
        );
        assert_eq!(
            standing(Badge::Count(0)).empty_text(Tab::Decided),
            s::EMPTY_DECIDED
        );
        assert_eq!(
            standing(Badge::Count(3)).top_line().split(' ').next(),
            Some("3")
        );
        assert!(standing(Badge::Count(1))
            .top_line()
            .ends_with(s::EVERY_ANSWER_FINAL));
    }

    #[test]
    fn the_waiting_tab_carries_the_count_and_the_unknown_mark() {
        let label = |b, t| standing(b).tab_label(t);
        assert_eq!(label(Badge::Count(2), Tab::Waiting), "Waiting 2");
        assert_eq!(label(Badge::Count(0), Tab::Waiting), "Waiting");
        assert_eq!(label(Badge::Unknown, Tab::Waiting), "Waiting ?");
        assert_eq!(label(Badge::Count(2), Tab::Decided), "Decided");
    }

    #[test]
    fn notices_name_what_cannot_be_trusted() {
        let mut st = standing(Badge::Count(1));
        assert!(st.notices(1).is_empty());
        st.badge = Badge::Unknown;
        assert_eq!(
            st.notices(0),
            Vec::<String>::new(),
            "an empty list says it in its own text"
        );
        assert_eq!(st.notices(2), [s::INCOMPLETE_NOTICE]);
        st.refused = 1;
        st.parked = 1;
        let n = st.notices(0);
        assert_eq!(n, [s::refused_notice(1), s::PARKED_NOTICE.to_owned()]);
        assert!(s::refused_notice(2).contains("2 requests"));
    }

    fn state_with(ids: &[&str]) -> ApprovalsState {
        let mut st = ApprovalsState::new();
        st.apply(A::FetchStarted(Scope::All, FetchId(1)));
        let rows = ids
            .iter()
            .map(|i| req(i, "sess", "create_file", "adds"))
            .collect();
        st.apply(A::Snapshot(rows, Scope::All, FetchId(1), T0));
        st
    }

    #[test]
    fn tab_visits_the_tabs_the_rows_then_the_cards_controls() {
        let mut st = state_with(&["a", "b"]);
        st.apply(A::Select(Some("a".into())));
        let rows = st.rows(Tab::Waiting, T0);
        let card = st.selected_card(T0);
        let stops = page_stops(&rows, card.as_ref());
        let ids: Vec<_> = rows.iter().map(|r| r.id.clone()).collect();
        assert_eq!(stops[..2], [PageStop::TabWaiting, PageStop::TabDecided]);
        assert_eq!(stops[2], PageStop::Row(ids[0].clone()));
        assert_eq!(stops[3], PageStop::Row(ids[1].clone()));
        assert_eq!(
            stops[4..],
            [PageStop::DontAllow, PageStop::AllowOnce, PageStop::Details]
        );
    }

    #[test]
    fn the_last_stop_is_never_an_answer_so_shift_tab_from_nothing_cannot_allow() {
        let mut st = state_with(&["a"]);
        st.apply(A::Select(Some("a".into())));
        let rows = st.rows(Tab::Waiting, T0);
        let card = st.selected_card(T0).unwrap();
        assert!(card.answerable());
        let stops = page_stops(&rows, Some(&card));
        assert_eq!(stops.last(), Some(&PageStop::Details));
        assert_eq!(stops.first(), Some(&PageStop::TabWaiting));
    }

    #[test]
    fn a_card_that_cannot_be_answered_has_no_button_to_reach() {
        let mut st = state_with(&["a"]);
        st.apply(A::Select(Some("a".into())));
        // Time is up: the card says why and the buttons are not stops.
        let late = T0 + 901_000;
        let rows = st.rows(Tab::Waiting, late);
        let card = st.selected_card(late).unwrap();
        assert!(!card.answerable());
        let stops = page_stops(&rows, Some(&card));
        assert!(!stops
            .iter()
            .any(|s| matches!(s, PageStop::DontAllow | PageStop::AllowOnce)));
        assert_eq!(stops.last(), Some(&PageStop::Details));
        // Nothing selected: no card controls at all.
        assert_eq!(page_stops(&[], None).len(), 2);
    }
}
