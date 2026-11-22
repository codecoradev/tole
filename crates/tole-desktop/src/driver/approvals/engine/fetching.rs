//! `GET /approvals`: when it starts, what its reply does, and what a loss does.
//!
//! The adapter says: after a loss apply `ConnectionLost`, then `follow_up`,
//! then call. Here the call is deferred by the pace (`not_before`) but the
//! model is told at once, so until a fetch that started after the loss
//! succeeds the badge is `Unknown` and every box `Checking`. The fetch is
//! started with `FetchIds::start`, which is what `follow_up` returns.

use tole_client::{ApprovalList, Error};
use tole_desktop_model::approvals::{ApprovalsAction, FetchId, Scope};

use super::{millis, Effect, Engine, Now, Out};
use crate::adapter::approvals::{failed, snapshot, RETRY_MIN};

impl Engine {
    /// The model is told the knowledge is lost, and a fetch is wanted. The
    /// first loss before a fetch starts also pushes that fetch back by the
    /// next backoff step (`bump`); more losses before it starts are the same
    /// loss, so a serve restart seen by four streams costs one step, not four.
    pub(super) fn lose(&mut self, now: Now, bump: bool) -> bool {
        let changed = self.store.lock().apply(ApprovalsAction::ConnectionLost);
        self.current = None;
        self.last_ok = None;
        if !self.want {
            if bump {
                let wait = millis(self.backoff.failed());
                self.not_before = self.not_before.max(now.mono.saturating_add(wait));
            }
            self.want = true;
        }
        changed
    }

    /// The wall clock moved further than the monotonic one: the machine slept
    /// (the monotonic clock stops in a suspend) or the clock was corrected
    /// forward. Whatever was known may be hours old, and a remote stream is
    /// dead: the list is not known until a fetch says so. Not scheduling: only
    /// this detection reads the wall delta.
    pub(super) fn check_jump(&mut self, now: Now) -> bool {
        let prev = self.last_seen.replace(now);
        let Some(prev) = prev else { return false };
        let wall = now.wall.saturating_sub(prev.wall);
        let mono = now.mono.saturating_sub(prev.mono);
        if wall > mono.saturating_add(super::JUMP_GAP_MS) {
            self.lose(now, false)
        } else {
            false
        }
    }

    /// The list is older than twice the refresh interval: refreshes fail or
    /// hang, so it is not known any more.
    pub(super) fn check_age(&mut self, now: Now) -> bool {
        let limit = millis(self.refresh).saturating_mul(2);
        match self.last_ok {
            Some(t) if now.mono.saturating_sub(t) > limit => self.lose(now, false),
            _ => false,
        }
    }

    /// Start the fetch that is wanted, when none is out and the pace allows.
    pub(super) fn maybe_fetch(&mut self, now: Now, out: &mut Out) -> bool {
        let allowed =
            self.want && self.current.is_none() && !self.fetch_busy && now.mono >= self.not_before;
        if !allowed {
            return false;
        }
        self.want = false;
        let Some((id, started)) = self.ids.start(Scope::All) else {
            return false; // the counter is spent: the list stays unknown
        };
        let changed = self.store.lock().apply(started);
        self.current = Some(id);
        self.fetch_busy = true;
        self.refresh_asap = false;
        self.fetch_epoch = self.epoch;
        // At most one fetch a second, whatever asks.
        self.not_before = now.mono.saturating_add(millis(RETRY_MIN));
        out.effects.push(Effect::Fetch(id));
        changed
    }

    /// The reply of fetch `id`. A reply of a fetch that is no longer the
    /// current one still goes to the model, which merges its rows and keeps
    /// the list as unknown as it was; it moves nothing else here.
    pub(super) fn fetched(
        &mut self,
        id: FetchId,
        reply: Result<ApprovalList, Error>,
        now: Now,
    ) -> bool {
        self.fetch_busy = false;
        let action = match &reply {
            Ok(list) => snapshot(list, Scope::All, id, now.wall),
            Err(_) => failed(Scope::All, id),
        };
        let mine = self.current == Some(id);
        let ok = reply.is_ok();
        let covered = mine && ok && self.covered(self.fetch_epoch);
        let mut changed = false;
        if mine {
            // The count first, then the state: a view that sees the list known
            // never sees the failures of the time before.
            self.failed_in_row = if ok {
                0
            } else {
                self.failed_in_row.saturating_add(1)
            };
            changed |= self.store.set_failures(self.failed_in_row);
        }
        {
            // One lock: the list is never seen known by a fetch that does not
            // count (it began before every followed stream was up, so a
            // request raised in between is in neither).
            let mut state = self.store.lock();
            changed |= state.apply(action);
            if mine && ok && !covered {
                changed |= state.apply(ApprovalsAction::ConnectionLost);
            }
        }
        if !mine {
            return changed;
        }
        self.current = None;
        if ok {
            self.want = !covered && self.all_up();
            self.next_refresh = Some(now.mono.saturating_add(millis(self.refresh)));
            if covered {
                self.backoff.succeeded();
                self.last_ok = Some(now.mono);
            }
        } else {
            let wait = millis(self.backoff.failed());
            self.not_before = self.not_before.max(now.mono.saturating_add(wait));
            self.want = true;
        }
        changed
    }
}
