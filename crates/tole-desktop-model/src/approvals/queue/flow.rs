//! How a request enters the queue and how its outcome is settled: frames,
//! snapshots, the knowledge of the list, the bounds and the tombstones.

use std::collections::HashSet;

use super::super::request::{ApprovalRequest, Outcome};
use super::super::wait::Wait;
use super::ingest::admit;
use super::{
    waiting, ApprovalsState, Entry, Overflow, Phase, Scope, Tombstones, MAX_DECIDED,
    MAX_KNOWN_SESSIONS, MAX_OVERFLOW, MAX_TOMBSTONES, MAX_WAITING,
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
        let r = admit(request);
        if r.is_none() && pending_like {
            self.refused = self.refused.saturating_add(1);
        }
        r
    }

    pub(super) fn requested(&mut self, request: ApprovalRequest, now: u64) -> bool {
        let Some(request) = self.admitted(request) else {
            return true; // counted
        };
        match Outcome::of_status(&request.status) {
            Some(outcome) => self.decide(request, outcome, now, false, now), // a decision
            None => self.pending(request, now),
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
                e.phase = phase; // revived
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
            return !std::mem::replace(&mut self.overflow_saturated, true);
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
    /// ones, newest first, at most [`MAX_DECIDED`], stamped with the time the
    /// request was made (the list does not say when it was decided): they never
    /// push out an answer given here. What the list omits is closed only if
    /// it was seen before the fetch started.
    pub(super) fn snapshot(&mut self, rows: Vec<ApprovalRequest>, scope: Scope, now: u64) -> bool {
        let mark = self.marks.remove(&scope);
        let seen: HashSet<String> = rows.iter().map(|r| r.id.clone()).collect();
        let mut changed = false;
        if scope == Scope::All {
            // Rebuilt from the list; what the stream announced since stays.
            let before = self.overflow.len();
            self.overflow.retain(|_, o| mark.is_none_or(|m| o.seq > m));
            changed |= self.overflow.len() != before;
            changed |= self.refused != 0;
            self.refused = 0;
        }
        // First, so the room it frees is there for the rows of the list.
        if let Some(mark) = mark {
            changed |= self.close_omitted(&scope, &seen, mark, now);
        }
        let (pending, mut decided): (Vec<_>, Vec<_>) = rows
            .into_iter()
            .partition(|r| Outcome::of_status(&r.status).is_none());
        for row in pending {
            if let Some(row) = self.admitted(row) {
                changed |= self.pending(row, now);
            } else {
                changed = true;
            }
        }
        decided.sort_by_key(|r| std::cmp::Reverse(r.created_at_ms));
        for row in decided.into_iter().take(MAX_DECIDED) {
            let Some(row) = self.admitted(row) else {
                continue;
            };
            let outcome = Outcome::of_status(&row.status).unwrap_or(Outcome::Closed);
            let at = row.created_at_ms.unwrap_or(0).min(now);
            changed |= self.decide(row, outcome, at, false, now);
        }
        match &scope {
            Scope::All => changed |= !std::mem::replace(&mut self.known_all, true),
            Scope::Session(id) if self.known_sessions.len() < MAX_KNOWN_SESSIONS => {
                changed |= self.known_sessions.insert(id.clone());
            }
            Scope::Session(_) => {}
        }
        changed |= std::mem::replace(&mut self.stale, false);
        changed
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
