//! What the UI reads from the skills driver: a cloned, plain-data copy of the
//! page state, with no toolkit type in it.

use tole_desktop_model::{Badge, SkillsPageState};

use super::store::{Feed, Shared};

/// The Skills page, the Connected tools page and the sidebar counts.
///
/// Read the rows and the badges from here, never from a guess: a tab that is
/// not loaded, whose last load failed, that the server could only read in part
/// or whose data is older than twice the refresh interval has no rows or a
/// partial list, and its badge is [`Badge::Unknown`], not a zero (the model's
/// rules, see [`SkillsPageState`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillsPageView {
    /// The tabs, their loads (`Idle` before the first call, `Loading` while the
    /// first one runs, `Ready`, `Failed` with a message), the rows and notes.
    pub page: SkillsPageState,
    /// The sidebar count of skills and drafts that wait for a decision.
    pub skills_waiting: Badge,
    /// The sidebar count of tools that changed or are not trusted yet.
    pub tools_needing_attention: Badge,
    /// The Skills tab's feed: failures in a row and whether a call is out.
    pub skills_feed: Feed,
    /// The Connected tools tab's feed.
    pub tools_feed: Feed,
}

impl SkillsPageView {
    /// What there is before a driver has run, and after it stopped: nothing
    /// loaded, both badges `Unknown`.
    pub fn not_loaded() -> Self {
        Self::of(&Shared::default())
    }

    pub(super) fn of(shared: &Shared) -> Self {
        SkillsPageView {
            page: shared.page.clone(),
            skills_waiting: shared.page.skills_waiting(),
            tools_needing_attention: shared.page.tools_needing_attention(),
            skills_feed: shared.skills,
            tools_feed: shared.tools,
        }
    }
}
