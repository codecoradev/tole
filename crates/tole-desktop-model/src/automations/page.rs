//! What the Automations page says about its list (board S6), as plain data.
//!
//! The page state knows what it knows ([`super::AutomationsState`]); this
//! module only words it: the line at the right of the top bar, what replaces
//! the table when there is nothing to show, and the notices about a list that
//! may not be complete or current. A list that was not read never reads as an
//! empty one, and a count is given only for a list that was read, is current
//! and is all of the jobs.

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
}

#[cfg(test)]
mod tests {
    use super::*;

    fn standing(load: Load) -> Standing {
        Standing {
            load,
            left_out: 0,
            counts: (2, 1),
        }
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
}
