//! Fetches and snapshots: which reply is trusted, what it closes, and what it
//! makes known.

use std::collections::HashSet;

use super::super::request::ApprovalRequest;
use super::super::request::Outcome;
use super::{
    waiting, ApprovalsState, FetchId, Mark, Phase, Scope, FETCH_TIMEOUT_MS, MAX_DECIDED,
    MAX_KNOWN_SESSIONS,
};

impl ApprovalsState {
    pub(super) fn fetch_started(&mut self, scope: Scope, id: FetchId) -> bool {
        if matches!(&scope, Scope::Session(_))
            && !self.marks.contains_key(&scope)
            && self.marks.len() >= MAX_KNOWN_SESSIONS
        {
            // No room: the oldest session fetch is given up (its reply is
            // untrusted from now on, so it can only fail toward unknown).
            let oldest = (self.marks.iter())
                .filter(|(s, _)| matches!(s, Scope::Session(_)))
                .min_by_key(|(_, m)| (m.seq, m.id.0))
                .map(|(s, _)| s.clone());
            match oldest {
                Some(old) => self.marks.remove(&old),
                None => return false,
            };
        }
        if scope == Scope::All {
            self.refetch = false;
        }
        let seq = self.seq;
        match self.marks.get_mut(&scope) {
            // A second fetch while one runs: its reply is the current one, but
            // what it may close is what the older mark could justify.
            Some(m) => {
                m.latest = seq;
                m.since = None;
                std::mem::replace(&mut m.id, id) != id
            }
            None => {
                let (latest, since) = (seq, None);
                self.marks.insert(
                    scope,
                    Mark {
                        seq,
                        latest,
                        id,
                        since,
                    },
                );
                true
            }
        }
    }

    /// Gives up the fetches that were started a `FETCH_TIMEOUT_SECS` ago
    /// and never ended: as after `FetchFailed`, what they cover is unknown
    /// and a late reply is not trusted, and the adapter is asked for a new
    /// fetch (`ApprovalsState::needs_fetch`). Without it a reply that never
    /// comes would leave every message box `Checking` and fill the table of
    /// marks.
    pub(super) fn watchdog(&mut self, now: u64) -> bool {
        let mut lapsed = Vec::new();
        for (scope, m) in &mut self.marks {
            match m.since {
                Some(t) if now >= t.saturating_add(FETCH_TIMEOUT_MS) => lapsed.push(scope.clone()),
                // A clock that stepped back times the fetch from here.
                Some(t) if now >= t => {}
                _ => m.since = Some(now),
            }
        }
        for scope in &lapsed {
            self.marks.remove(scope);
            match scope {
                Scope::All => self.known_all = false,
                Scope::Session(id) => {
                    self.known_sessions.remove(id);
                }
            }
        }
        self.refetch |= !lapsed.is_empty();
        !lapsed.is_empty()
    }

    pub(super) fn fetch_failed(&mut self, scope: &Scope, id: FetchId) -> bool {
        if self.marks.get(scope).is_none_or(|m| m.id != id) {
            return false; // not the current fetch: it says nothing about now
        }
        self.marks.remove(scope);
        match scope {
            Scope::All => std::mem::replace(&mut self.known_all, false),
            Scope::Session(id) => self.known_sessions.remove(id),
        }
    }

    pub(super) fn connection_lost(&mut self) -> bool {
        self.marks.clear();
        let had = self.known_all || !self.known_sessions.is_empty();
        self.known_all = false;
        self.known_sessions.clear();
        had
    }

    /// A whole list from the server, the reply of fetch `id`. Pending rows are
    /// taken first (a request that needs an answer must not lose its place to
    /// history), then decided ones: those of requests that wait here (or only
    /// guessed an outcome) always, the others newest first and at most
    /// [`MAX_DECIDED`], ranked below anything decided here. A snapshot that
    /// answers the current fetch of its scope (a marked one) also closes what
    /// the list omits and the queue saw before the fetch started, discounts the
    /// refusals and the overflow of that time, shows what it lists as pending
    /// (the server's word) and makes the list known. Any other snapshot only
    /// merges its rows, and leaves what is known as it was.
    pub(super) fn snapshot(
        &mut self,
        rows: Vec<ApprovalRequest>,
        scope: Scope,
        id: FetchId,
        now: u64,
    ) -> bool {
        let current = self.marks.get(&scope).is_some_and(|m| m.id == id);
        let (mark, latest) = match current.then(|| self.marks.remove(&scope)).flatten() {
            Some(m) => (Some(m.seq), m.latest),
            None => (None, 0),
        };
        let had_selection = self.selected.is_some();
        let seen: HashSet<String> = rows.iter().map(|r| r.id.clone()).collect();
        let mut changed = false;
        if let Some(mark) = mark {
            changed |= self.account_for(&scope, mark);
            // First, so the room it frees is there for the rows of the list.
            changed |= self.close_omitted(&scope, &seen, mark);
        }
        let (pending, decided): (Vec<_>, Vec<_>) = rows
            .into_iter()
            .partition(|r| Outcome::of_status(&r.status).is_none());
        let (mine, mut rest): (Vec<_>, Vec<_>) = decided.into_iter().partition(|r| {
            (self.entries.get(&r.id)).is_some_and(|e| {
                waiting(e)
                    || matches!(
                        e.phase,
                        Phase::Decided {
                            provisional: true,
                            ..
                        }
                    )
            }) || self.overflow.contains_key(&r.id)
        });
        for row in pending {
            // A reply that is not the current fetch is older than the queue's
            // own knowledge. It revives nothing, and while a current fetch
            // runs it adds nothing and refuses nothing either: a request it
            // lists as pending may be decided already, and that fetch could
            // not close (or account for) what is created after it started.
            // The stream and the current reply show what is pending, and
            // refuse what is malformed.
            if mark.is_none()
                && (self.entries.contains_key(&row.id)
                    || self.fetch_outstanding(row.session_id.as_deref()))
            {
                continue;
            }
            match self.admitted(row, now) {
                Some(row) => {
                    let appeared = self.pending(row, now, mark);
                    changed |= appeared;
                    // A selection this snapshot is about to clear is not
                    // replaced by another card: nobody read that one.
                    if appeared && !had_selection {
                        self.auto_select();
                    }
                }
                None => changed = true,
            }
        }
        rest.sort_by_key(|r| std::cmp::Reverse(r.created_at_ms));
        rest.truncate(MAX_DECIDED);
        let imported = |r: &ApprovalRequest| (false, r.created_at_ms.unwrap_or(0).min(now));
        let rows =
            (mine.into_iter().map(|r| (r, true))).chain(rest.into_iter().map(|r| (r, false)));
        for (row, here) in rows {
            let Some(row) = self.admitted(row, now) else {
                continue;
            };
            let outcome = Outcome::of_status(&row.status).unwrap_or(Outcome::Closed);
            let rank = if here {
                self.local_rank()
            } else {
                imported(&row)
            };
            changed |= self.decide(row, outcome, rank, false, now);
        }
        if let Some(mark) = mark {
            match &scope {
                Scope::All => {
                    changed |= !std::mem::replace(&mut self.known_all, true);
                    // A conflict after the fetch started is not in its reply.
                    if mark >= self.stale_since {
                        changed |= std::mem::replace(&mut self.stale, false);
                    }
                    self.refetch |= self.left_between(mark, latest, &seen);
                }
                Scope::Session(id) if self.known_sessions.len() < MAX_KNOWN_SESSIONS => {
                    changed |= self.known_sessions.insert(id.clone());
                }
                Scope::Session(_) => {}
            }
        }
        changed |= self.refresh_if_room();
        changed
    }

    /// Whether a fetch that covers `session` runs: its reply is the one that
    /// closes what is created now, if it were created below its mark.
    fn fetch_outstanding(&self, session: Option<&str>) -> bool {
        !self.marks.is_empty()
            && (self.marks.contains_key(&Scope::All)
                || session.is_some_and(|s| self.marks.contains_key(&Scope::Session(s.to_owned()))))
    }

    /// Whether something that came between the first start (`mark`) and the
    /// newest (`latest`) of overlapping fetches is still unaccounted for after
    /// the reply of the newest: a refusal, a request over the cap, or a waiting
    /// request the reply omits. Only the older mark is trusted for those, so
    /// the adapter is asked for one more fetch.
    fn left_between(&self, mark: u64, latest: u64, seen: &HashSet<String>) -> bool {
        let between = |seq: u64| seq > mark && seq <= latest;
        latest > mark
            && (self.refusals.iter().any(|r| between(r.seq))
                || self.overflow.values().any(|o| between(o.seq))
                || (self.entries.iter())
                    .any(|(id, e)| waiting(e) && between(e.seq) && !seen.contains(id)))
    }

    /// What a fetch that started at `mark` accounts for: the requests over the
    /// cap, the refused rows and the loss of count that it could have listed
    /// (a full list is rebuilt from the reply; what came later stays).
    fn account_for(&mut self, scope: &Scope, mark: u64) -> bool {
        let (over, refused) = (self.overflow.len(), self.refusals.len());
        match scope {
            Scope::All => {
                self.overflow.retain(|_, o| o.seq > mark);
                self.refusals.retain(|r| r.seq > mark);
            }
            Scope::Session(id) => {
                let ours = |s: Option<&str>| s == Some(id.as_str());
                self.overflow
                    .retain(|_, o| o.seq > mark || !ours(o.session.as_deref()));
                self.refusals
                    .retain(|r| r.seq > mark || !ours(r.session.as_deref()));
            }
        }
        let lost = *scope == Scope::All && self.saturated.is_some_and(|s| s <= mark);
        if lost {
            self.saturated = None;
        }
        lost || over != self.overflow.len() || refused != self.refusals.len()
    }

    /// Waiting requests of `scope` the list omits and the queue saw before the
    /// fetch started: no longer waiting, outcome unknown (a guess).
    fn close_omitted(&mut self, scope: &Scope, seen: &HashSet<String>, mark: u64) -> bool {
        let in_scope = |session: Option<&str>| match scope {
            Scope::All => true,
            Scope::Session(id) => session == Some(id.as_str()),
        };
        let before = self.overflow.len();
        self.overflow
            .retain(|id, o| seen.contains(id) || o.seq > mark || !in_scope(o.session.as_deref()));
        let mut changed = self.overflow.len() != before;
        let omitted = self.ordered(|id, e| {
            waiting(e)
                && e.seq <= mark
                && !seen.contains(id)
                && in_scope(e.request.session_id.as_deref())
        });
        for id in omitted {
            let rank = self.local_rank();
            if let Some(e) = self.entries.get_mut(&id) {
                e.phase = Phase::Decided {
                    outcome: Outcome::Closed,
                    rank,
                    provisional: true,
                    answer_lost: false,
                };
                changed = true;
            }
        }
        self.trim_decided();
        changed
    }
}
