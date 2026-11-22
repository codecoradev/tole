//! How a request enters the queue and how its outcome is settled: frames,
//! snapshots, the knowledge of the list, the bounds and the tombstones.

use super::super::request::{ApprovalRequest, Outcome};
use super::super::wait::{Wait, EXPIRY_SECS, SKEW_MS};
use super::ingest::{admit, valid_key, MAX_SESSION_ID_LEN};
use super::{
    waiting, ApprovalsState, Entry, Overflow, Phase, Rank, Refusal, Tombstones, MAX_DECIDED,
    MAX_OVERFLOW, MAX_REFUSALS, MAX_TOMBSTONES, MAX_WAITING,
};

impl Tombstones {
    fn add(&mut self, id: String, guess: bool) {
        if self.ids.insert(id.clone(), guess).is_none() {
            self.order.push_back(id);
            if self.order.len() > MAX_TOMBSTONES {
                if let Some(old) = self.order.pop_front() {
                    self.ids.remove(&old);
                }
            }
        }
    }

    fn has(&self, id: &str) -> bool {
        self.ids.contains_key(id)
    }

    /// The id was trimmed while it was only a guess (not the server's word).
    fn was_guess(&self, id: &str) -> bool {
        self.ids.get(id).copied().unwrap_or(false)
    }

    fn remove(&mut self, id: &str) {
        if self.ids.remove(id).is_some() {
            self.order.retain(|x| x != id);
        }
    }
}

impl ApprovalsState {
    /// A row that passed [`admit`], or a count of the refusal (only a row that
    /// may be pending matters: it could be a request nobody can answer). Such
    /// a request cannot wait past the rule's 900 s (plus the skew of the two
    /// clocks) from `now`, so the refusal is forgotten then.
    pub(super) fn admitted(
        &mut self,
        request: ApprovalRequest,
        now: u64,
    ) -> Option<ApprovalRequest> {
        let pending_like = Outcome::of_status(&request.status).is_none()
            || !Outcome::status_is_known(&request.status);
        let session = request.session_id.clone();
        let r = admit(request);
        if r.is_none() && pending_like {
            let until = now.saturating_add(EXPIRY_SECS * 1000 + SKEW_MS);
            self.refuse(session, until);
        }
        r
    }

    /// Remembers a refused row that may be pending: the task it names is not
    /// known to be free. An unusable session id could be any task's. `until`
    /// is when the request can no longer be pending (`now_ms`).
    fn refuse(&mut self, session: Option<String>, until: u64) {
        let (session, any_session) = match session.filter(|s| !s.is_empty()) {
            None => (None, false),
            Some(s) if valid_key(&s, MAX_SESSION_ID_LEN) => (Some(s), false),
            Some(_) => (None, true),
        };
        self.add_refusal(Refusal {
            seq: 0,
            session,
            any_session,
            until,
        });
    }

    fn add_refusal(&mut self, mut refusal: Refusal) {
        self.seq += 1;
        refusal.seq = self.seq;
        if self.refusals.len() >= MAX_REFUSALS {
            // No room to name it: the newest stands for all of them, and for
            // as long as the longest of the two.
            let last = self.refusals.len() - 1;
            refusal.until = refusal.until.max(self.refusals[last].until);
            self.refusals[last] = Refusal {
                session: None,
                any_session: true,
                ..refusal
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
        let Some(request) = self.admitted(request, now) else {
            return true; // counted
        };
        match Outcome::of_status(&request.status) {
            Some(outcome) => self.decide_here(request, outcome, now), // a decision
            None => {
                let appeared = self.pending(request, now, None);
                if appeared {
                    self.auto_select();
                }
                appeared
            }
        }
    }

    pub(super) fn resolved(&mut self, request: ApprovalRequest, now: u64) -> bool {
        let Some(request) = self.admitted(request, now) else {
            return true;
        };
        // A frame that says "pending" is not a decision.
        match Outcome::of_status(&request.status) {
            Some(outcome) => self.decide_here(request, outcome, now),
            None => false,
        }
    }

    fn waiting_listed(&self) -> usize {
        self.entries.values().filter(|e| waiting(e)).count()
    }

    /// A request the server says is pending. A repeat changes nothing; a
    /// decision that was only a guess and is still inside its wait is revived;
    /// a trimmed id never comes back. `fetch`: the row is in the reply of the
    /// current fetch that started at this `seq`, the server's word after that
    /// start: it also revives a guess whose wait is over (the local clock may
    /// run ahead) and an id trimmed as a guess, with the skew as grace; but
    /// not a request this queue settled after the fetch started (a conflict):
    /// the reply cannot know it.
    pub(super) fn pending(
        &mut self,
        request: ApprovalRequest,
        now: u64,
        fetch: Option<u64>,
    ) -> bool {
        let server_word = fetch.is_some();
        let seq = self.seq + 1; // taken only if the request is kept
        let full = self.waiting_listed() >= MAX_WAITING;
        let id = request.id.clone();
        let known = (self.entries.get(&id)).map(|e| {
            let guess = matches!(
                e.phase,
                Phase::Decided {
                    provisional: true,
                    ..
                }
            );
            (guess, e.wait, e.seq)
        });
        let mut contradicted = false;
        let wait =
            match known {
                Some((true, wait, seq)) if fetch.map_or(!wait.expired(now), |mark| seq <= mark) => {
                    contradicted = wait.expired(now);
                    wait
                }
                Some(_) => return false, // a repeat after a reconnect: the first row stays
                None if self.tombstones.has(&id) => {
                    if !(server_word && self.tombstones.was_guess(&id)) {
                        return false;
                    }
                    contradicted = true;
                    Wait::new(request.created_at_ms, request.expires_in_secs, now)
                }
                None => (self.overflow.get(&id).map(|o| o.wait)) // promoted: its own wait
                    .unwrap_or_else(|| {
                        Wait::new(request.created_at_ms, request.expires_in_secs, now)
                    }),
            };
        let wait = match contradicted {
            true => wait.at_least(now.saturating_add(SKEW_MS)),
            false => wait,
        };
        if full {
            return self.overflowed(&request, wait, seq);
        }
        self.seq = seq;
        let phase = Phase::Waiting { answering: None };
        match self.entries.get_mut(&id) {
            Some(e) => {
                e.request = request; // revived: the server's pending row
                e.wait = wait;
                e.phase = phase;
                e.seq = seq;
            }
            None => {
                self.tombstones.remove(&id);
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

    pub(super) fn local_rank(&mut self) -> Rank {
        self.decided_seq += 1;
        (true, self.decided_seq)
    }

    /// Settles a request the server decided now, as far as this queue knows.
    pub(super) fn decide_here(
        &mut self,
        request: ApprovalRequest,
        outcome: Outcome,
        now: u64,
    ) -> bool {
        let rank = self.local_rank();
        self.decide(request, outcome, rank, false, now)
    }

    /// Settles a request. The server's decision replaces a guess; any other
    /// decided request keeps its first answer, and a guess (also a status this
    /// version does not know) never replaces another decision. `rank` orders
    /// it in the history, `now` is the adapter's clock.
    pub(super) fn decide(
        &mut self,
        request: ApprovalRequest,
        outcome: Outcome,
        rank: Rank,
        guess: bool,
        now: u64,
    ) -> bool {
        let provisional = guess || !Outcome::status_is_known(&request.status);
        let phase = Phase::Decided {
            outcome,
            rank,
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

    /// Drops the oldest decided requests over [`MAX_DECIDED`] (by [`Rank`], not
    /// by the clock) and remembers their ids. A dropped request whose status
    /// this version did not know may have been pending: the list is not
    /// trusted until the next full snapshot.
    pub(super) fn trim_decided(&mut self) {
        let mut decided: Vec<(Rank, u64, &String)> = (self.entries.iter())
            .filter_map(|(id, e)| match e.phase {
                Phase::Decided { rank, .. } => Some((rank, e.seq, id)),
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
            let Some(e) = self.entries.remove(&id) else {
                continue;
            };
            let guess = matches!(
                e.phase,
                Phase::Decided {
                    provisional: true,
                    ..
                }
            );
            if guess && !Outcome::status_is_known(&e.request.status) {
                // It may have been pending, for any task, until its wait ends.
                self.add_refusal(Refusal {
                    seq: 0,
                    session: None,
                    any_session: true,
                    until: e.wait.deadline_ms(),
                });
            }
            self.tombstones.add(id, guess);
        }
    }
}
