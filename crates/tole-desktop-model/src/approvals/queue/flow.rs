//! How a request enters the queue and how its outcome is settled: frames,
//! snapshots, the knowledge of the list, the bounds and the tombstones.

use std::collections::HashSet;

use super::super::request::{ApprovalRequest, Outcome};
use super::super::wait::Wait;
use super::ingest::{admit, valid_key, MAX_SESSION_ID_LEN};
use super::{
    waiting, ApprovalsState, Entry, Overflow, Phase, Refusal, Scope, Tombstones, MAX_DECIDED,
    MAX_KNOWN_SESSIONS, MAX_OVERFLOW, MAX_REFUSALS, MAX_TOMBSTONES, MAX_WAITING,
};

impl Tombstones {
    fn add(&mut self, id: String) {
        if self.ids.insert(id.clone()) {
            self.order.push_back(id);
            if self.order.len() > MAX_TOMBSTONES {
                if let Some(old) = self.order.pop_front() {
                    self.ids.remove(&old);
                }
            }
        }
    }

    fn has(&self, id: &str) -> bool {
        self.ids.contains(id)
    }
}

impl ApprovalsState {
    /// A row that passed [`admit`], or a count of the refusal (only a row that
    /// may be pending matters: it could be a request nobody can answer).
    fn admitted(&mut self, request: ApprovalRequest) -> Option<ApprovalRequest> {
        let pending_like = Outcome::of_status(&request.status).is_none()
            || !Outcome::status_is_known(&request.status);
        let session = request.session_id.clone();
        let r = admit(request);
        if r.is_none() && pending_like {
            self.refuse(session);
        }
        r
    }

    /// Remembers a refused row that may be pending: the task it names is not
    /// known to be free. An unusable session id could be any task's.
    fn refuse(&mut self, session: Option<String>) {
        self.seq += 1;
        let seq = self.seq;
        let refusal = match session.filter(|s| !s.is_empty()) {
            None => Refusal {
                seq,
                session: None,
                any_session: false,
            },
            Some(s) if valid_key(&s, MAX_SESSION_ID_LEN) => Refusal {
                seq,
                session: Some(s),
                any_session: false,
            },
            Some(_) => Refusal {
                seq,
                session: None,
                any_session: true,
            },
        };
        if self.refusals.len() >= MAX_REFUSALS {
            // No room to name it: the newest stands for all of them.
            let last = self.refusals.len() - 1;
            self.refusals[last] = Refusal {
                seq,
                session: None,
                any_session: true,
            };
        } else {
            self.refusals.push(refusal);
        }
    }

    /// Asks for a refresh when room frees while requests over the cap are
    /// uncounted: the fetch lists them and they get a card.
    pub(super) fn refresh_if_room(&mut self) -> bool {
        if self.stale || (self.overflow.is_empty() && self.saturated.is_none()) {
            return false;
        }
        self.stale = self.waiting_listed() < MAX_WAITING;
        self.stale
    }

    pub(super) fn requested(&mut self, request: ApprovalRequest, now: u64) -> bool {
        let Some(request) = self.admitted(request) else {
            return true; // counted
        };
        match Outcome::of_status(&request.status) {
            Some(outcome) => self.decide(request, outcome, now, false, now), // a decision
            None => {
                let appeared = self.pending(request, now);
                if appeared {
                    self.auto_select();
                }
                appeared
            }
        }
    }

    pub(super) fn resolved(&mut self, request: ApprovalRequest, now: u64) -> bool {
        let Some(request) = self.admitted(request) else {
            return true;
        };
        // A frame that says "pending" is not a decision.
        match Outcome::of_status(&request.status) {
            Some(outcome) => self.decide(request, outcome, now, false, now),
            None => false,
        }
    }

    fn waiting_listed(&self) -> usize {
        self.entries.values().filter(|e| waiting(e)).count()
    }

    /// A request the server says is pending. A repeat changes nothing; a
    /// decision that was only a guess and is still inside its wait is revived;
    /// a trimmed id never comes back.
    fn pending(&mut self, request: ApprovalRequest, now: u64) -> bool {
        let seq = self.seq + 1; // taken only if the request is kept
        let full = self.waiting_listed() >= MAX_WAITING;
        let known = (self.entries.get(&request.id)).map(|e| {
            let guess = matches!(
                e.phase,
                Phase::Decided {
                    provisional: true,
                    ..
                }
            );
            (guess, e.wait)
        });
        let wait = match known {
            Some((guess, wait)) if guess && !wait.expired(now) => wait,
            Some(_) => return false, // a repeat after a reconnect: the first row stays
            None if self.tombstones.has(&request.id) => return false,
            None => Wait::new(request.created_at_ms, request.expires_in_secs, now),
        };
        if full {
            return self.overflowed(&request, wait, seq);
        }
        self.seq = seq;
        let phase = Phase::Waiting { answering: None };
        let id = request.id.clone();
        match self.entries.get_mut(&id) {
            Some(e) => {
                e.request = request; // revived: the server's pending row
                e.phase = phase;
                e.seq = seq;
            }
            None => {
                self.entries.insert(
                    id.clone(),
                    Entry {
                        request,
                        wait,
                        phase,
                        seq,
                    },
                );
            }
        }
        self.overflow.remove(&id);
        true
    }

    /// Counts a waiting request that has no room; its task still blocks.
    fn overflowed(&mut self, request: &ApprovalRequest, wait: Wait, seq: u64) -> bool {
        if self.overflow.contains_key(&request.id) {
            return false;
        }
        if self.overflow.len() >= MAX_OVERFLOW {
            self.seq = seq;
            return self.saturated.replace(seq).is_none();
        }
        self.seq = seq;
        let session = request.session_id.clone();
        self.overflow
            .insert(request.id.clone(), Overflow { session, wait, seq });
        true
    }

    /// Settles a request. The server's decision replaces a guess; any other
    /// decided request keeps its first answer, and a guess (also a status this
    /// version does not know) never replaces another decision. `at_ms` is when
    /// it was decided, `now` the adapter's clock.
    pub(super) fn decide(
        &mut self,
        request: ApprovalRequest,
        outcome: Outcome,
        at_ms: u64,
        guess: bool,
        now: u64,
    ) -> bool {
        let provisional = guess || !Outcome::status_is_known(&request.status);
        let phase = Phase::Decided {
            outcome,
            at_ms,
            provisional,
            answer_lost: false,
        };
        let mut changed = self.overflow.remove(&request.id).is_some();
        match self.entries.get_mut(&request.id) {
            Some(e) => {
                let final_ = matches!(
                    e.phase,
                    Phase::Decided {
                        provisional: false,
                        ..
                    }
                );
                if final_ || (provisional && !waiting(e)) || e.phase == phase {
                    return changed;
                }
                e.request.status = request.status; // the card reads the status too
                e.phase = phase;
            }
            None if self.tombstones.has(&request.id) => return changed,
            None => {
                self.seq += 1;
                let seq = self.seq;
                let wait = Wait::new(request.created_at_ms, request.expires_in_secs, now);
                let id = request.id.clone();
                self.entries.insert(
                    id,
                    Entry {
                        request,
                        wait,
                        phase,
                        seq,
                    },
                );
            }
        }
        changed = true;
        self.trim_decided();
        changed
    }

    /// Drops the oldest decided requests over [`MAX_DECIDED`] and remembers
    /// their ids.
    pub(super) fn trim_decided(&mut self) {
        let mut decided: Vec<(u64, u64, &String)> = (self.entries.iter())
            .filter_map(|(id, e)| match e.phase {
                Phase::Decided { at_ms, .. } => Some((at_ms, e.seq, id)),
                Phase::Waiting { .. } => None,
            })
            .collect();
        let Some(over) = decided.len().checked_sub(MAX_DECIDED).filter(|n| *n > 0) else {
            return;
        };
        decided.sort_unstable();
        let gone: Vec<String> = decided
            .into_iter()
            .take(over)
            .map(|(_, _, id)| id.clone())
            .collect();
        for id in gone {
            self.entries.remove(&id);
            self.tombstones.add(id);
        }
    }

    pub(super) fn fetch_started(&mut self, scope: Scope) -> bool {
        if matches!(&scope, Scope::Session(_))
            && !self.marks.contains_key(&scope)
            && self.marks.len() >= MAX_KNOWN_SESSIONS
        {
            return false; // no room to tell: its snapshot will close nothing
        }
        let seq = self.seq;
        self.marks.insert(scope, seq) != Some(seq)
    }

    pub(super) fn fetch_failed(&mut self, scope: &Scope) -> bool {
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

    /// A whole list from the server. Pending rows are taken first (a request
    /// that needs an answer must not lose its place to history), then decided
    /// ones: those of requests that wait here always, the others newest first
    /// and at most [`MAX_DECIDED`], stamped with the time the request was made
    /// (the list does not say when it was decided): they never push out an
    /// answer given here. A snapshot that answers a `FetchStarted` (a marked
    /// one) also closes what the list omits and the queue saw before the
    /// fetch started, discounts the refusals and the overflow of that time,
    /// and makes the list known. An unmarked one only merges its rows.
    pub(super) fn snapshot(&mut self, rows: Vec<ApprovalRequest>, scope: Scope, now: u64) -> bool {
        let mark = self.marks.remove(&scope);
        let seen: HashSet<String> = rows.iter().map(|r| r.id.clone()).collect();
        let mut changed = false;
        if let Some(mark) = mark {
            changed |= self.account_for(&scope, mark);
            // First, so the room it frees is there for the rows of the list.
            changed |= self.close_omitted(&scope, &seen, mark, now);
        }
        let (pending, decided): (Vec<_>, Vec<_>) = rows
            .into_iter()
            .partition(|r| Outcome::of_status(&r.status).is_none());
        let (mine, mut rest): (Vec<_>, Vec<_>) = decided.into_iter().partition(|r| {
            (self.entries.get(&r.id)).is_some_and(waiting) || self.overflow.contains_key(&r.id)
        });
        for row in pending {
            match self.admitted(row) {
                Some(row) => {
                    let appeared = self.pending(row, now);
                    changed |= appeared;
                    if appeared {
                        self.auto_select();
                    }
                }
                None => changed = true,
            }
        }
        rest.sort_by_key(|r| std::cmp::Reverse(r.created_at_ms));
        rest.truncate(MAX_DECIDED);
        for row in mine.into_iter().chain(rest) {
            let Some(row) = self.admitted(row) else {
                continue;
            };
            let outcome = Outcome::of_status(&row.status).unwrap_or(Outcome::Closed);
            let at = row.created_at_ms.unwrap_or(0).min(now);
            changed |= self.decide(row, outcome, at, false, now);
        }
        if mark.is_some() {
            match &scope {
                Scope::All => changed |= !std::mem::replace(&mut self.known_all, true),
                Scope::Session(id) if self.known_sessions.len() < MAX_KNOWN_SESSIONS => {
                    changed |= self.known_sessions.insert(id.clone());
                }
                Scope::Session(_) => {}
            }
            changed |= std::mem::replace(&mut self.stale, false);
        }
        changed |= self.refresh_if_room();
        changed
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
    fn close_omitted(
        &mut self,
        scope: &Scope,
        seen: &HashSet<String>,
        mark: u64,
        now: u64,
    ) -> bool {
        let in_scope = |session: Option<&str>| match scope {
            Scope::All => true,
            Scope::Session(id) => session == Some(id.as_str()),
        };
        let before = self.overflow.len();
        self.overflow
            .retain(|id, o| seen.contains(id) || o.seq > mark || !in_scope(o.session.as_deref()));
        let mut changed = self.overflow.len() != before;
        for (id, e) in &mut self.entries {
            if waiting(e)
                && e.seq <= mark
                && !seen.contains(id)
                && in_scope(e.request.session_id.as_deref())
            {
                e.phase = Phase::Decided {
                    outcome: Outcome::Closed,
                    at_ms: now,
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
