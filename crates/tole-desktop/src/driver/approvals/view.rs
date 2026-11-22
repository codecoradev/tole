//! What the UI reads from the approvals driver: a cloned, plain-data view of
//! the model, worked out at the time it is asked for (so the countdowns are
//! current), with no toolkit type in it.

use tole_desktop_model::approvals::{ApprovalCard, ApprovalsState, ComposerState, Row, Tab};
use tole_desktop_model::Badge;

/// The approvals page and the sidebar, as of one moment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApprovalsView {
    /// The sidebar badge: a number only when the whole list is known.
    pub badge: Badge,
    /// At least this many requests wait; draw "N+" while the badge is `Unknown`.
    pub waiting_at_least: usize,
    /// Rows refused as malformed and not yet accounted for (a protocol problem).
    pub refused: usize,
    /// Fetches of the whole list that failed in a row, 0 when the last one
    /// worked (or none was tried): the page can say Tole cannot be reached and
    /// that it keeps trying. The retry waits 1 s, then doubles up to 30 s.
    pub fetch_failures: u32,
    /// Followed sessions whose stream the serve keeps refusing (404 until the
    /// task is resumed, `closed`, 429): the list is known without them, and a
    /// request of theirs can be up to one refresh interval late. A page can say
    /// "checked every N seconds" for these. Their streams are probed slowly.
    /// Also the sessions added with `Handle::set_sessions` that wait for a
    /// free stream slot (see "Changing the followed sessions" in the module
    /// docs of the driver).
    pub parked_sessions: Vec<String>,
    /// The tab shown.
    pub tab: Tab,
    /// The rows of the tab shown.
    pub rows: Vec<Row>,
    /// The card the page draws: the selected request, if the user picked one.
    pub selected: Option<ApprovalCard>,
    /// The message box of every task that has a request waiting. A task not
    /// listed here has nothing waiting: ask [`Handle::composer`](super::Handle::composer),
    /// which is `Checking` until the list is known.
    pub composers: Vec<(String, ComposerState)>,
}

impl ApprovalsView {
    pub(super) fn of(
        state: &ApprovalsState,
        now: u64,
        fetch_failures: u32,
        parked_sessions: Vec<String>,
    ) -> Self {
        let waiting = state.rows(Tab::Waiting, now);
        let mut composers: Vec<(String, ComposerState)> = Vec::new();
        for row in &waiting {
            if let Some(task) = &row.session_id {
                if !composers.iter().any(|(t, _)| t == task) {
                    composers.push((task.clone(), state.composer(task)));
                }
            }
        }
        let tab = state.tab();
        ApprovalsView {
            badge: state.waiting_badge(),
            waiting_at_least: state.waiting_at_least(),
            refused: state.refused(),
            fetch_failures,
            parked_sessions,
            tab,
            rows: match tab {
                Tab::Waiting => waiting,
                Tab::Decided => state.rows(Tab::Decided, now),
            },
            selected: state.selected_card(now),
            composers,
        }
    }
}
