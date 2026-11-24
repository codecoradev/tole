//! What the UI reads from the automations driver: a cloned, plain-data copy of
//! the page state, with no toolkit type in it.

use tole_desktop_model::{AutomationId, AutomationsState, Load};

use super::store::{Feed, History, Shared};

/// The Automations page.
///
/// Read the rows from here, never from a guess: before the first answer there
/// are none, and a list that could not be read again or is older than twice the
/// refresh keeps its rows but `page.load` is `Failed` (draw them as "may be out
/// of date", and the scheduler line as not running: the model has already set
/// it). Nothing here is a number the page may show as a known count unless
/// [`counts`](Self::counts) gives it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AutomationsView {
    /// The rows, the scheduler line, the selection and its history entries, the
    /// notice under the table (a refused switch) and whether a plan-mode
    /// server refuses every resume.
    pub page: AutomationsState,
    /// Jobs of the answer that are not among `page.rows` (past the cap, or an
    /// id an earlier row has). Above zero the table is not all of them: draw
    /// "N more not shown".
    pub left_out: usize,
    /// The list's feed: failures in a row and whether a call is out.
    pub jobs_feed: Feed,
    /// The history of `page.selected`: its load, what was left out, whether
    /// the approval links are known, its feed.
    pub history: History,
    /// The automation whose switch waits for the serve's answer: draw it busy.
    /// The switch itself still shows the state the serve last reported.
    pub toggling: Option<AutomationId>,
}

impl AutomationsView {
    /// What there is before a driver has run, and after it stopped: no rows,
    /// not loaded, no count.
    pub fn not_loaded() -> Self {
        Self::of(&Shared::default())
    }

    pub(super) fn of(shared: &Shared) -> Self {
        AutomationsView {
            page: shared.page.clone(),
            left_out: shared.left_out,
            jobs_feed: shared.jobs,
            history: shared.history.clone(),
            toggling: shared.toggling.clone(),
        }
    }

    /// (on, paused) for the headline ("2 on, 1 paused"), only when the rows are
    /// a list that was read and is all of the jobs: `None` before the first
    /// answer, while the list is out of date or failed, and when jobs were left
    /// out.
    pub fn counts(&self) -> Option<(usize, usize)> {
        (self.page.load == Load::Ready && self.left_out == 0).then(|| self.page.counts())
    }
}
