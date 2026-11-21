//! Which reply of `GET /skills` and `GET /tools` may reach the page.
//!
//! Two calls of the same route can overlap (a refresh while a periodic load is
//! in flight), and the older answer can arrive last. Applied, it would replace
//! the newer page: a skill that changed since could show as approved, and the
//! badge would count one less, with no sign of it. The model says a list that
//! may be out of date is never drawn as the current trust state, so each call
//! gets an id when it starts and only the reply of the latest call of its tab is
//! applied; any other is ignored (the page keeps what the latest call set,
//! `Loading` while it runs).

use tole_client::{Error, SkillsState, ToolsState};
use tole_desktop_model::{Load, SkillsPageState};

use super::{apply_skills, apply_tools};

/// The identity of one call of a route, from [`LoadIds`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LoadId(u64);

/// The counters of the two tabs. One per `SkillsPageState`, owned with it and
/// shared by everything that loads for it (not `Clone`).
///
/// Ids never repeat. After `u64::MAX` calls of a tab (never reached in
/// practice) `skills_started` or `tools_started` give `None` and leave the page
/// alone, so nothing new can be applied and the page keeps its last state.
#[derive(Debug, Default)]
pub struct LoadIds {
    skills: u64,
    tools: u64,
}

impl LoadIds {
    pub fn new() -> Self {
        Self::default()
    }

    /// The call of `GET /skills` is about to start: the Skills tab is
    /// `Loading` and this id is the latest. Pass it back with the reply.
    pub fn skills_started(&mut self, page: &mut SkillsPageState) -> Option<LoadId> {
        self.skills = self.skills.checked_add(1)?;
        page.skills_load = Load::Loading;
        Some(LoadId(self.skills))
    }

    /// The same for `GET /tools`.
    pub fn tools_started(&mut self, page: &mut SkillsPageState) -> Option<LoadId> {
        self.tools = self.tools.checked_add(1)?;
        page.tools_load = Load::Loading;
        Some(LoadId(self.tools))
    }

    /// The reply of the call `id` of `Client::skills`. Applied only when it is
    /// the latest call (the answer replaces the Skills tab, `notes` included, so
    /// a partial answer is a partial page; an error empties it and says why);
    /// `false` when it was ignored as late.
    pub fn apply_skills(
        &self,
        page: &mut SkillsPageState,
        id: LoadId,
        result: &Result<SkillsState, Error>,
    ) -> bool {
        let latest = id == LoadId(self.skills);
        if latest {
            apply_skills(page, result);
        }
        latest
    }

    /// The same for `Client::tools`.
    pub fn apply_tools(
        &self,
        page: &mut SkillsPageState,
        id: LoadId,
        result: &Result<ToolsState, Error>,
    ) -> bool {
        let latest = id == LoadId(self.tools);
        if latest {
            apply_tools(page, result);
        }
        latest
    }
}
