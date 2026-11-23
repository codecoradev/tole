//! `GET /cron/jobs`: when a call starts, what its answer does to the page and
//! what a loss of knowledge does.
//!
//! The page shows rows while `page.load` is `Ready`, and keeps them (marked
//! `Failed`) when a read fails or goes stale: the model draws old rows under a
//! failed marker, never as the current list. A call that starts on a `Ready`
//! page leaves it `Ready` (a refresh of a few milliseconds must not flicker);
//! `Feed::loading` says a call is out.

use tole_client::{CronJobs, Error};
use tole_desktop_model::Load;

use super::{CallId, Effect, Engine, Now, Out, STALE};
use crate::adapter::automations::{apply_jobs, JobsApplied};
use crate::driver::automations::store::History;

impl Engine {
    /// Start the list call that is wanted, when none is out and the pace
    /// allows. Returns whether the view changed.
    pub(super) fn begin_jobs(&mut self, now: Now, out: &mut Out) -> bool {
        if !self.jobs.may_start(now.mono) {
            return false;
        }
        let Some(id) = self.calls.next() else {
            // The counter is spent (never in practice): nothing new can be
            // applied, so the page keeps what it has.
            self.jobs.want = false;
            return false;
        };
        {
            let mut shared = self.store.lock();
            if shared.page.load == Load::Idle {
                shared.page.load = Load::Loading;
            }
            shared.jobs.loading = true;
        }
        self.jobs.started(id, now.mono);
        out.effects.push(Effect::LoadJobs(id));
        true
    }

    /// The answer of list call `id`. One of a call that was dropped (the
    /// machine slept while it was out, the state was thrown away, or a switch
    /// was answered after it started) is read for the slot it frees and
    /// nothing more: it describes the time before the change.
    pub(super) fn jobs_loaded(
        &mut self,
        id: CallId,
        result: Result<CronJobs, Error>,
        now: Now,
    ) -> bool {
        if self.jobs.out != Some(id) {
            // Not the call this route awaits (a repeat, a stranger): it frees
            // nothing and says nothing.
            return false;
        }
        self.jobs.out = None;
        if self.jobs.current != Some(id) {
            let mut shared = self.store.lock();
            return std::mem::replace(&mut shared.jobs.loading, false);
        }
        // Mapping and cleaning the strings of a large answer takes time, so it
        // is done on a copy of the page with no lock held (only this thread
        // changes the page), and the copy is put in place under the lock.
        let mut page = self.store.lock().page.clone();
        let applied = apply_jobs(&mut page, &result);
        let mut shared = self.store.lock();
        shared.page = page;
        shared.jobs.loading = false;
        match applied {
            JobsApplied::Ready { left_out } => {
                shared.left_out = left_out;
                shared.jobs.failures = 0;
                self.jobs.succeeded(now.mono, self.refresh);
            }
            JobsApplied::Failed => {
                shared.jobs.failures = shared.jobs.failures.saturating_add(1);
                self.jobs.failed(now.mono);
            }
        }
        if shared.page.selected.is_none() && shared.history.load != Load::Idle {
            // The selected automation is gone from the list: its history too.
            shared.history = History::default();
            drop(shared);
            self.runs.lost(true);
            self.runs.want = false;
        }
        true
    }

    /// What the list showed is not to be trusted any more: its rows stay and
    /// the page says why (a page that shows nothing has nothing to mark).
    pub(super) fn lose_jobs(&mut self, drop_call: bool) -> bool {
        let mut shared = self.store.lock();
        let shown = shared.page.load == Load::Ready;
        if shown {
            shared.page.load_failed(STALE);
        }
        drop(shared);
        self.jobs.lost(drop_call);
        shown
    }
}
