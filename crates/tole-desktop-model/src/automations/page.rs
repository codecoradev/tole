//! What the Automations page says about its list (board S6), as plain data.
//!
//! The page state knows what it knows ([`super::AutomationsState`]); this
//! module only words it: the line at the right of the top bar, what replaces
//! the table when there is nothing to show, the notices about a list that may
//! not be complete or current, the lines about the history of the selected
//! automation, and the order Tab visits the controls in. A list that was not
//! read never reads as an empty one, and a count is given only for a list
//! that was read, is current and is all of the jobs.

use super::job::{AutomationId, AutomationRow};
use super::run::RunEntry;
use crate::plain::Load;
use crate::strings::automations as s;

/// What the page knows about the list it shows: the fields of the driver's
/// view that bear on whether the list can be trusted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Standing {
    /// Where the list is: not asked, asked, read, or the last read failed
    /// (the rows, if any, are then the last ones read).
    pub load: Load,
    /// Jobs of the answer that are not among the rows.
    pub left_out: usize,
    /// (on, paused) of the rows.
    pub counts: (usize, usize),
    /// Where the history of the selected automation is.
    pub history: Load,
    /// At least this many older runs were not kept.
    pub history_more_at_least: usize,
    /// Whether the link of every run that stopped to ask could be looked up.
    pub links_known: bool,
}

impl Standing {
    /// Whether the rows are all of the jobs and current: the only list a count
    /// may be given for.
    pub fn known(&self) -> bool {
        self.load == Load::Ready && self.left_out == 0
    }

    /// "2 on, 1 paused" for the right of the top bar; nothing for a list that
    /// is not known, never a zero.
    pub fn top_line(&self) -> Option<String> {
        self.known()
            .then(|| s::summary(self.counts.0, self.counts.1))
    }

    /// The line that stands where the table would be, when there is nothing
    /// to draw in it: checking, could not read, or the empty list, which is
    /// said only for a list that was read.
    pub fn table_text(&self, rows_shown: usize) -> Option<&'static str> {
        if rows_shown > 0 {
            return None;
        }
        Some(match self.load {
            Load::Idle | Load::Loading => s::CHECKING,
            Load::Failed(_) => s::LOAD_FAILED,
            Load::Ready => s::EMPTY,
        })
    }

    /// Lines about what the list cannot be trusted for, under the table: a
    /// list that may be out of date, and the jobs the table has no room for.
    pub fn notices(&self, rows_shown: usize) -> Vec<String> {
        let mut out = Vec::new();
        if matches!(self.load, Load::Failed(_)) && rows_shown > 0 {
            out.push(s::OUT_OF_DATE.to_owned());
        }
        if self.left_out > 0 {
            out.push(s::more_not_shown(self.left_out));
        }
        out
    }

    /// The line that stands where the runs would be, when there are none to
    /// draw: reading, could not read, or "nothing has run" (said only for a
    /// history that was read).
    pub fn history_text(&self, entries_shown: usize) -> Option<&'static str> {
        if entries_shown > 0 {
            return None;
        }
        match self.history {
            Load::Idle => None,
            Load::Loading => Some(s::HISTORY_LOADING),
            Load::Failed(_) => Some(s::HISTORY_FAILED),
            Load::Ready => Some(s::HISTORY_EMPTY),
        }
    }

    /// Lines under the runs: older runs that were not kept, and that the link
    /// to Approvals of a run that stopped to ask is not known.
    pub fn history_notices(&self, entries: &[RunEntry]) -> Vec<String> {
        let mut out = Vec::new();
        if self.history_more_at_least > 0 {
            out.push(s::history_more(self.history_more_at_least));
        }
        let waiting_without_link = entries.iter().any(|e| {
            e.outcome == super::run::RunOutcome::StoppedToAsk && e.approval_link().is_none()
        });
        if !self.links_known && waiting_without_link {
            out.push(s::LINKS_UNKNOWN.to_owned());
        }
        out
    }
}

/// A control of the page that Tab visits.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum PageStop {
    /// A row of the table, by id: Enter shows its history.
    Row(AutomationId),
    /// The switch of a row, by id.
    Switch(AutomationId),
    /// The notice under the table: its dismiss button.
    DismissNotice,
    /// "Open in Approvals" of a run, by its fire id.
    OpenInApprovals(String),
}

/// The controls Tab visits, first to last: for each row the row and then its
/// switch, the dismiss button of the notice when there is one, and the links
/// to Approvals of the runs that stopped to ask and whose request is known.
/// A run with no known link has no stop: a key never reaches a link that goes
/// nowhere.
pub fn page_stops(rows: &[AutomationRow], notice: bool, history: &[RunEntry]) -> Vec<PageStop> {
    let mut stops = Vec::new();
    for row in rows {
        stops.push(PageStop::Row(row.id.clone()));
        stops.push(PageStop::Switch(row.id.clone()));
    }
    if notice {
        stops.push(PageStop::DismissNotice);
    }
    stops.extend(
        history
            .iter()
            .filter(|e| e.approval_link().is_some())
            .map(|e| PageStop::OpenInApprovals(e.fire_id.clone())),
    );
    stops
}

#[cfg(test)]
mod tests {
    use super::super::job::raw_job;
    use super::super::run::RawRun;
    use super::*;

    fn standing(load: Load) -> Standing {
        Standing {
            load,
            left_out: 0,
            counts: (2, 1),
            history: Load::Idle,
            history_more_at_least: 0,
            links_known: true,
        }
    }

    fn run(fire: &str, status: Option<&str>, approval: Option<&str>) -> RunEntry {
        RunEntry::new(RawRun {
            fire_id: fire.into(),
            scheduled_for: 1_700_000_000,
            status: status.map(str::to_owned),
            session_id: Some("s1".into()),
            approval_id: approval.map(str::to_owned),
            output: String::new(),
            output_truncated: false,
        })
    }

    #[test]
    fn a_list_that_was_not_read_never_reads_as_empty_or_as_a_count() {
        for load in [Load::Idle, Load::Loading] {
            let st = standing(load);
            assert_eq!(st.table_text(0), Some(s::CHECKING));
            assert_eq!(st.top_line(), None);
            assert!(!st.known());
        }
        let failed = standing(Load::Failed("down".into()));
        assert_eq!(failed.table_text(0), Some(s::LOAD_FAILED));
        assert_eq!(failed.top_line(), None, "never a count of a failed read");
    }

    #[test]
    fn a_list_that_was_read_says_empty_only_when_it_is() {
        let st = standing(Load::Ready);
        assert_eq!(st.table_text(0), Some(s::EMPTY));
        assert_eq!(st.table_text(3), None, "rows are drawn, no line");
        assert_eq!(st.top_line().as_deref(), Some("2 on, 1 paused"));
        assert!(st.notices(3).is_empty());
    }

    #[test]
    fn a_failed_read_keeps_its_rows_and_says_they_may_be_out_of_date() {
        let st = standing(Load::Failed("down".into()));
        assert_eq!(st.table_text(2), None);
        assert_eq!(st.notices(2), vec![s::OUT_OF_DATE.to_owned()]);
        assert!(st.notices(0).is_empty(), "nothing to be out of date");
    }

    #[test]
    fn jobs_the_table_has_no_room_for_are_counted_and_make_the_headline_unknown() {
        let mut st = standing(Load::Ready);
        st.left_out = 3;
        assert!(!st.known());
        assert_eq!(st.top_line(), None, "the rows are only some of the jobs");
        assert_eq!(st.notices(5), vec!["3 more not shown".to_owned()]);
    }

    #[test]
    fn the_history_says_nothing_has_run_only_for_a_history_that_was_read() {
        let mut st = standing(Load::Ready);
        assert_eq!(st.history_text(0), None, "nothing is selected");
        st.history = Load::Loading;
        assert_eq!(st.history_text(0), Some(s::HISTORY_LOADING));
        st.history = Load::Failed("down".into());
        assert_eq!(st.history_text(0), Some(s::HISTORY_FAILED));
        st.history = Load::Ready;
        assert_eq!(st.history_text(0), Some(s::HISTORY_EMPTY));
        assert_eq!(st.history_text(2), None, "runs are drawn, no line");
    }

    #[test]
    fn the_history_names_what_it_does_not_show_and_what_it_does_not_know() {
        let mut st = standing(Load::Ready);
        st.history = Load::Ready;
        st.history_more_at_least = 5;
        let asked = run("f1", Some("awaiting_approval"), None);
        let passed = run("f2", Some("ok"), None);
        // Links known: a run with no link just has none.
        assert_eq!(
            st.history_notices(&[asked.clone(), passed.clone()]),
            vec!["At least 5 older runs are not shown".to_owned()]
        );
        st.links_known = false;
        assert_eq!(
            st.history_notices(std::slice::from_ref(&passed)).len(),
            1,
            "no run asked"
        );
        let notes = st.history_notices(&[asked, passed]);
        assert_eq!(notes.len(), 2);
        assert_eq!(notes[1], s::LINKS_UNKNOWN);
    }

    #[test]
    fn tab_visits_each_row_and_its_switch_then_the_notice_then_the_links() {
        let rows = vec![
            AutomationRow::new(raw_job("a", "cli", "active", false)),
            AutomationRow::new(raw_job("b", "cli", "paused", false)),
        ];
        let history = vec![
            run("f1", Some("awaiting_approval"), Some("ap1")),
            run("f2", Some("ok"), None),
            run("f3", Some("awaiting_approval"), None),
        ];
        let a = AutomationId::new("a");
        let b = AutomationId::new("b");
        assert_eq!(
            page_stops(&rows, true, &history),
            vec![
                PageStop::Row(a.clone()),
                PageStop::Switch(a.clone()),
                PageStop::Row(b.clone()),
                PageStop::Switch(b.clone()),
                PageStop::DismissNotice,
                PageStop::OpenInApprovals("f1".into()),
            ],
            "only a run that asked and has a known request has a link"
        );
        assert_eq!(
            page_stops(&rows, false, &[]),
            vec![
                PageStop::Row(a.clone()),
                PageStop::Switch(a),
                PageStop::Row(b.clone()),
                PageStop::Switch(b)
            ],
            "no notice, no history: no more stops"
        );
        assert!(page_stops(&[], false, &[]).is_empty());
    }
}
