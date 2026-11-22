//! `GET /skills` and `GET /tools`: when a call starts, what its answer does to
//! the page and what a loss of knowledge does.
//!
//! The page shows a list while its tab is `Ready`. A call that starts on a
//! `Ready` tab leaves it `Ready` (rows and badge stay, `Feed::loading` says a
//! call is out): flipping the badge to `Unknown` for every refresh of a few
//! milliseconds would make it flicker, and the age rule (`Engine::step`) bounds
//! how old the rows can get. A tab that is not `Ready` shows `Loading`.

use tole_client::{Error, SkillsState, ToolsState};
use tole_desktop_model::{Load, SkillsPageState};

use super::{Effect, Engine, Now, Out, Route, STALE};
use crate::adapter::skills::LoadId;
use crate::driver::skills::store::{Feed, Shared};

/// The answer of a call of one route.
pub(in crate::driver::skills) enum Reply {
    Skills(Result<SkillsState, Error>),
    Tools(Result<ToolsState, Error>),
}

impl Reply {
    pub(super) fn route(&self) -> Route {
        match self {
            Reply::Skills(_) => Route::Skills,
            Reply::Tools(_) => Route::Tools,
        }
    }

    fn is_ok(&self) -> bool {
        match self {
            Reply::Skills(r) => r.is_ok(),
            Reply::Tools(r) => r.is_ok(),
        }
    }
}

fn load_of(page: &SkillsPageState, route: Route) -> &Load {
    match route {
        Route::Skills => &page.skills_load,
        Route::Tools => &page.tools_load,
    }
}

fn feed_of(shared: &mut Shared, route: Route) -> &mut Feed {
    match route {
        Route::Skills => &mut shared.skills,
        Route::Tools => &mut shared.tools,
    }
}

impl Engine {
    /// Start the call of `route` that is wanted, when none is out and the pace
    /// allows. Returns whether the view changed.
    pub(super) fn begin(&mut self, route: Route, now: Now, out: &mut Out) -> bool {
        if !self.lanes[route.index()].may_start(now.mono) {
            return false;
        }
        let id = {
            let mut shared = self.store.lock();
            let was = load_of(&shared.page, route).clone();
            let id = match route {
                Route::Skills => self.ids.skills_started(&mut shared.page),
                Route::Tools => self.ids.tools_started(&mut shared.page),
            };
            let Some(id) = id else {
                // The counter is spent (never in practice): nothing new can
                // be applied, so the page keeps what it has.
                self.lanes[route.index()].want = false;
                return false;
            };
            if was == Load::Ready {
                match route {
                    Route::Skills => shared.page.skills_load = Load::Ready,
                    Route::Tools => shared.page.tools_load = Load::Ready,
                }
            }
            feed_of(&mut shared, route).loading = true;
            id
        };
        self.lanes[route.index()].started(id, now.mono);
        out.effects.push(Effect::Load(route, id));
        true
    }

    /// The answer of call `id`. One of a call that was dropped (the machine
    /// slept while it was out, or the state was thrown away) is read for the
    /// slot it frees and nothing more: it describes the time before the loss.
    pub(super) fn loaded(&mut self, id: LoadId, reply: Reply, now: Now) -> bool {
        let route = reply.route();
        let lane = &mut self.lanes[route.index()];
        if lane.out != Some(id) {
            // Not the call this route awaits (a repeat, a stranger): it frees
            // nothing and says nothing.
            return false;
        }
        lane.out = None;
        let mine = lane.current == Some(id);
        if !mine {
            let mut shared = self.store.lock();
            return std::mem::replace(&mut feed_of(&mut shared, route).loading, false);
        }
        let ok = reply.is_ok();
        // The latest call of its tab only (`LoadIds`): the page takes it, an
        // error included (rows gone, the reason shown). Mapping and cleaning
        // the strings of a large answer takes time, so it is done on a copy of
        // the page with no lock held (only this thread changes the page), and
        // the copy is put in place under the lock.
        let mut page = self.store.lock().page.clone();
        let applied = match &reply {
            Reply::Skills(r) => self.ids.apply_skills(&mut page, id, r),
            Reply::Tools(r) => self.ids.apply_tools(&mut page, id, r),
        };
        let mut shared = self.store.lock();
        shared.page = page;
        let feed = feed_of(&mut shared, route);
        feed.loading = false;
        if ok && applied {
            feed.failures = 0;
            lane.succeeded(now.mono, self.refresh);
        } else {
            feed.failures = feed.failures.saturating_add(1);
            lane.failed(now.mono);
        }
        true
    }

    /// What `route` showed is not to be trusted any more: its rows go and the
    /// page says why (a tab that shows nothing has nothing to take back).
    pub(super) fn lose(&mut self, route: Route, drop_call: bool) -> bool {
        let mut shared = self.store.lock();
        let shown = *load_of(&shared.page, route) == Load::Ready;
        if shown {
            match route {
                Route::Skills => shared.page.skills_failed(STALE),
                Route::Tools => shared.page.tools_failed(STALE),
            }
        }
        drop(shared);
        self.lanes[route.index()].lost(drop_call);
        shown
    }
}
