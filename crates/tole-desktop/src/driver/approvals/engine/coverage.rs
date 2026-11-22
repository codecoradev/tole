//! Which followed streams are up, and when the list may be called known.
//!
//! The model's badge and boxes are only true if every announcement reaches the
//! queue. For a followed session that holds while its stream is open, and a
//! fetch only covers what was raised before it, so the list is known when a
//! fetch that STARTED after every followed stream was up has succeeded. Any
//! change of which streams are up bumps `epoch`; a fetch remembers the epoch it
//! started in, and its success counts only if that is still the epoch.
//!
//! A session that is not followed has no stream: a request of it is found by
//! the periodic refresh, at most one interval late. So is one that is parked: a
//! stream the serve refuses again and again (404 after a serve restart, until
//! the owner resumes the session; `closed`; 429 when the serve's stream slots
//! are taken) must not hold the whole app unknown, but must not be forgotten
//! either: it is listed in the view and its reader probes it slowly.

use super::{Cover, Engine, Now};

impl Engine {
    /// Every followed stream is up, or parked (and so not waited for).
    pub(super) fn all_up(&self) -> bool {
        self.followed.values().all(|c| *c != Cover::Down)
    }

    /// A fetch that started in `epoch` is trustworthy now.
    pub(super) fn covered(&self, epoch: u64) -> bool {
        self.all_up() && epoch == self.epoch
    }

    fn set_cover(&mut self, session: &str, to: Cover) -> bool {
        let Some(cover) = self.followed.get_mut(session) else {
            return false;
        };
        *cover = to;
        self.epoch = self.epoch.wrapping_add(1);
        let parked: Vec<String> = (self.followed.iter())
            .filter(|(_, c)| **c == Cover::Parked)
            .map(|(s, _)| s.clone())
            .collect();
        self.store.set_parked(parked)
    }

    /// The stream delivered its first item, so it is subscribed: a fetch from
    /// now on covers everything raised before it. A parked session rejoins.
    pub(super) fn stream_up(&mut self, session: &str) -> bool {
        let changed = self.set_cover(session, Cover::Up);
        if self.all_up() {
            self.want = true;
        }
        changed
    }

    /// The stream ended or failed: knowledge is lost until it is back up and a
    /// fetch has followed.
    pub(super) fn stream_down(&mut self, session: &str, now: Now) -> bool {
        if !self.followed.contains_key(session) {
            return false;
        }
        let parked = self.set_cover(session, Cover::Down);
        self.lose(now, true) | parked
    }

    /// The stream is refused again and again: it stops holding the list
    /// unknown. Its requests are found by the refresh, so up to one refresh
    /// interval late; the view lists it (`parked_sessions`) so a page can say
    /// so. Its reader keeps probing and rejoins with `stream_up`.
    pub(super) fn stream_parked(&mut self, session: &str) -> bool {
        let changed = self.set_cover(session, Cover::Parked);
        if self.all_up() {
            self.want = true;
        }
        changed
    }
}
