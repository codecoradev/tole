//! What the UI reads from the backend: plain data, with no toolkit type and no
//! lock behind it.

use tole_desktop_model::ServeState;

use crate::driver::approvals::ApprovalsView;
use crate::driver::skills::SkillsPageView;

/// The serve and what its drivers know, as of one moment. A clone: the
/// caller holds nothing of the backend while it uses it.
///
/// The drivers' parts read as the drivers say when nothing is known
/// ([`ApprovalsView::unknown`], [`SkillsPageView::not_loaded`]) whenever there
/// is no driver for the serve that is there now: before the first attach,
/// while the serve is not ready, when it was replaced and the drivers have
/// not caught up, and after the backend stopped. Never a zero in their place.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BackendView {
    pub serve: ServeState,
    pub approvals: ApprovalsView,
    pub skills: SkillsPageView,
    /// The tasks the app has open, in the order it opened them: the app's
    /// wish, which holds while the serve is down. Read each one by
    /// [`Backend::task_view`](super::Backend::task_view).
    pub open_tasks: Vec<String>,
}
