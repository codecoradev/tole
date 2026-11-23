//! `GET /cron/runs` of the selected automation: the selection, when a call
//! starts and what its answer does to the history.

use tole_desktop_model::{AutomationId, Load};

use super::{CallId, Effect, Engine, Now, Out, RunsReply, STALE};
use crate::adapter::automations::{apply_runs, RunsApplied};
use crate::driver::automations::store::History;

impl Engine {
    /// Show the history of `id` (its exact text), or close it with `None`.
    /// Selecting the one that is open asks for a fresh read and keeps what is
    /// shown; selecting another empties the history until its first answer; an
    /// id that is not a row of the page selects nothing.
    pub(super) fn select(&mut self, id: Option<String>) -> bool {
        let mut shared = self.store.lock();
        match id {
            Some(raw) => {
                let id = AutomationId::new(&raw);
                if shared.page.selected.as_ref() == Some(&id) {
                    drop(shared);
                    self.runs.asap = true;
                    return false;
                }
                if !shared.page.select(&id) {
                    return false;
                }
                shared.history = History {
                    load: Load::Loading,
                    ..History::default()
                };
                drop(shared);
                // The call that may be out is for the old selection: its answer
                // is ignored, and the new one is asked for at once.
                self.runs.lost(true);
                self.runs.asap = true;
                true
            }
            None => {
                if shared.page.selected.is_none() {
                    return false;
                }
                shared.page.selected = None;
                shared.page.history.clear();
                shared.history = History::default();
                drop(shared);
                self.runs.lost(true);
                self.runs.want = false;
                true
            }
        }
    }

    /// Start the history call that is wanted, when an automation is selected,
    /// none is out and the pace allows. Returns whether the view changed.
    pub(super) fn begin_runs(&mut self, now: Now, out: &mut Out) -> bool {
        if !self.runs.may_start(now.mono) {
            return false;
        }
        let Some(job) = self.store.lock().page.selected.clone() else {
            self.runs.want = false;
            return false;
        };
        let Some(id) = self.calls.next() else {
            self.runs.want = false;
            return false;
        };
        {
            let mut shared = self.store.lock();
            if shared.history.load == Load::Idle {
                shared.history.load = Load::Loading;
            }
            shared.history.feed.loading = true;
        }
        self.runs.started(id, now.mono);
        out.effects.push(Effect::LoadRuns(id, job));
        true
    }

    /// The answer of history call `id`. One for a call that was dropped, or for
    /// an automation that is no longer the selected one, frees the slot and
    /// changes nothing.
    pub(super) fn runs_loaded(&mut self, id: CallId, reply: RunsReply, now: Now) -> bool {
        if self.runs.out != Some(id) {
            return false;
        }
        self.runs.out = None;
        let mine = self.runs.current == Some(id);
        let RunsReply { job, runs, lookup } = reply;
        let shared = self.store.lock();
        if !mine || shared.page.selected.as_ref() != Some(&job) {
            let mut shared = shared;
            let changed = std::mem::replace(&mut shared.history.feed.loading, false);
            drop(shared);
            if mine {
                // An answer for something else than what is selected: ask again.
                self.runs.lost(true);
            }
            return changed;
        }
        let mut page = shared.page.clone();
        drop(shared);
        let applied = apply_runs(&mut page, &job, &runs, lookup.as_ref());
        let mut shared = self.store.lock();
        shared.page = page;
        shared.history.feed.loading = false;
        match applied {
            Some(RunsApplied::Ready {
                more_at_least,
                links_known,
            }) => {
                shared.history.load = Load::Ready;
                shared.history.more_at_least = more_at_least;
                shared.history.links_known = links_known;
                shared.history.feed.failures = 0;
                self.runs.succeeded(now.mono, self.refresh);
            }
            Some(RunsApplied::Failed(message)) => {
                shared.history.load = Load::Failed(message);
                shared.history.feed.failures = shared.history.feed.failures.saturating_add(1);
                self.runs.failed(now.mono);
            }
            None => {
                // Not reachable (the selection was checked above); not a
                // success either way.
                shared.history.feed.failures = shared.history.feed.failures.saturating_add(1);
                self.runs.failed(now.mono);
            }
        }
        true
    }

    /// The history shown is not to be trusted any more: its entries stay and
    /// it is marked failed.
    pub(super) fn lose_runs(&mut self, drop_call: bool) -> bool {
        let mut shared = self.store.lock();
        let shown = shared.history.load == Load::Ready;
        if shown {
            shared.history.load = Load::Failed(STALE.to_owned());
        }
        drop(shared);
        self.runs.lost(drop_call);
        shown
    }
}
