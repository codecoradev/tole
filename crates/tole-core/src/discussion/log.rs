//! The discussion ledger (#588): every step of a discussion is one line in
//! an append-only JSONL file, and the state, including what comes next, is
//! folded from it. A crash mid-discussion resumes from the file; a line is
//! never rewritten.
//!
//! Every provider call is paid for in two steps: [`DiscussionLog::reserve`]
//! before the call counts its estimate against the budget (or, when it does
//! not fit, records the budget escalation: a hard stop, never a silent end),
//! and [`DiscussionLog::settle`] after it replaces the estimate with what was
//! spent. A call a crash interrupted stays counted at its estimate, so a
//! resumed discussion never spends more than its budget allows, and a
//! finished step is never run again because the ledger's [`Next`] has moved
//! past it.
//!
//! The log is also the state machine's referee. Each step is checked against
//! [`Next`], what the log says must happen now: a contribution only from a
//! participant who has not spoken this round, a summary only once everyone has,
//! the checker's verdicts only after the summary, and the gate's result is
//! computed here from the ledger rather than accepted from the caller, so no
//! step can be skipped, repeated or forged by the code driving the
//! discussion.

use crate::discussion::{
    gate, is_small, Contract, Contribution, CriterionVerdict, Gate, GateInput, Reason,
    RoundSummary, MAX_FALLBACK_REASON_CHARS,
};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::io::Write as _;
use std::path::{Path, PathBuf};

/// Largest log read back or written: the per-discussion cap.
pub const MAX_LOG_BYTES: u64 = 16 * 1024 * 1024;
/// Largest single record (one line).
pub const MAX_RECORD_BYTES: usize = 512 * 1024;
/// Room kept at the end of a full log for the gate and the closing record,
/// so a discussion can always be stopped and closed.
pub const CLOSE_RESERVE: u64 = 128 * 1024;
/// Longest closing text kept, in characters.
pub const MAX_CLOSE_CHARS: usize = 16_000;
/// Longest call kind (`what`) and handle of a reservation.
pub const MAX_WHAT_CHARS: usize = 64;
pub const MAX_HANDLE_CHARS: usize = 128;
/// The discussions directory in the tole data dir.
pub const DISCUSSIONS_DIR: &str = "discussions";

/// One thing that happened.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum Event {
    Contract(Contract),
    /// Who speaks from `round` on, a subset of the contract's participants.
    Roster {
        round: u32,
        participants: Vec<String>,
    },
    Contribution(Contribution),
    Summary(RoundSummary),
    Verdicts {
        round: u32,
        verdicts: Vec<CriterionVerdict>,
    },
    /// Tokens the discussion spent (any role).
    Usage {
        round: u32,
        tokens: u64,
    },
    /// A provider call about to run in `round`, counted at its estimate
    /// until settled. `what` says which call (`participant-<name>`,
    /// `scribe`, `checker`).
    Reserve {
        round: u32,
        call: String,
        what: String,
        tokens: u64,
    },
    /// The host's handle of a reserved call that runs elsewhere (a
    /// delegated child's node), so a resumed host polls it instead of
    /// starting it again.
    Bind {
        call: String,
        handle: String,
    },
    /// What a reserved call spent; it replaces the estimate.
    Settle {
        call: String,
        tokens: u64,
    },
    /// What the host saw after `round` that only it can see.
    Flags {
        round: u32,
        owner_only_disagreement: bool,
        pending_approval: bool,
    },
    Gate {
        round: u32,
        outcome: Gate,
    },
    /// The end: the gate's outcome and the text handed to the owner.
    Closed {
        outcome: Gate,
        text: String,
    },
}

/// One line of the file.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct Line {
    seq: u64,
    t: u64,
    #[serde(flatten)]
    event: Event,
}

/// What must happen next.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Next {
    /// Nothing is recorded yet.
    NeedContract,
    /// These participants have not spoken in `round`.
    Contribute {
        round: u32,
        missing: Vec<String>,
    },
    /// `round` is complete. A small one is passed on as written
    /// (`verbatim`), a big one goes to the scribe.
    Summarize {
        round: u32,
        verbatim: bool,
    },
    /// The checker's verdicts on the criteria.
    Check {
        round: u32,
    },
    /// Compute the gate.
    Decide {
        round: u32,
    },
    /// The gate said report or escalate: write the closing text.
    Finish {
        outcome: Gate,
    },
    Done,
}

/// A reserved call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reservation {
    pub round: u32,
    pub what: String,
    pub tokens: u64,
    pub handle: Option<String>,
    pub settled: Option<u64>,
}

/// The answer to [`DiscussionLog::reserve`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Reserve {
    /// Reserved under this call id: run the call, then settle it.
    Go(String),
    /// It did not fit what is left: the gate's escalation is recorded and
    /// the discussion goes to [`Next::Finish`].
    Stop(Gate),
}

/// The folded log.
#[derive(Debug, Clone, Default)]
pub struct State {
    pub contract: Option<Contract>,
    rosters: BTreeMap<u32, Vec<String>>,
    pub contributions: Vec<Contribution>,
    pub summaries: BTreeMap<u32, RoundSummary>,
    pub verdicts: BTreeMap<u32, Vec<CriterionVerdict>>,
    /// Settled spend, plus every unsettled reservation at its estimate.
    pub tokens_used: u64,
    pub reservations: BTreeMap<String, Reservation>,
    flags: BTreeMap<u32, (bool, bool)>,
    pub gates: BTreeMap<u32, Gate>,
    pub closed: Option<(Gate, String)>,
    last_seq: u64,
}

impl State {
    fn fold(events: impl IntoIterator<Item = Event>) -> Self {
        let mut s = State::default();
        for e in events {
            s.apply(e);
        }
        s
    }

    fn apply(&mut self, e: Event) {
        self.last_seq += 1;
        match e {
            Event::Contract(c) => self.contract = Some(c),
            Event::Roster {
                round,
                participants,
            } => {
                self.rosters.insert(round, participants);
            }
            Event::Contribution(c) => self.contributions.push(c),
            Event::Summary(s) => {
                self.summaries.insert(s.round(), s);
            }
            Event::Verdicts { round, verdicts } => {
                self.verdicts.insert(round, verdicts);
            }
            Event::Usage { tokens, .. } => {
                self.tokens_used = self.tokens_used.saturating_add(tokens)
            }
            Event::Reserve {
                round,
                call,
                what,
                tokens,
            } => {
                self.tokens_used = self.tokens_used.saturating_add(tokens);
                self.reservations.insert(
                    call,
                    Reservation {
                        round,
                        what,
                        tokens,
                        handle: None,
                        settled: None,
                    },
                );
            }
            Event::Bind { call, handle } => {
                if let Some(r) = self.reservations.get_mut(&call) {
                    r.handle = Some(handle);
                }
            }
            Event::Settle { call, tokens } => {
                if let Some(r) = self.reservations.get_mut(&call) {
                    if r.settled.is_none() {
                        self.tokens_used = self
                            .tokens_used
                            .saturating_sub(r.tokens)
                            .saturating_add(tokens);
                        r.settled = Some(tokens);
                    }
                }
            }
            Event::Flags {
                round,
                owner_only_disagreement,
                pending_approval,
            } => {
                self.flags
                    .insert(round, (owner_only_disagreement, pending_approval));
            }
            Event::Gate { round, outcome } => {
                self.gates.insert(round, outcome);
            }
            Event::Closed { outcome, text } => self.closed = Some((outcome, text)),
        }
    }

    /// The participants who speak in `round`: the latest roster at or before
    /// it, else the contract's.
    pub fn roster(&self, round: u32) -> Vec<String> {
        self.rosters
            .range(..=round)
            .next_back()
            .map(|(_, p)| p.clone())
            .or_else(|| self.contract.as_ref().map(|c| c.participants.clone()))
            .unwrap_or_default()
    }

    /// Contributions of `round`.
    pub fn round_contributions(&self, round: u32) -> Vec<Contribution> {
        self.contributions
            .iter()
            .filter(|c| c.round == round)
            .cloned()
            .collect()
    }

    /// Every contribution id so far.
    pub fn known_entries(&self) -> BTreeSet<String> {
        self.contributions.iter().map(|c| c.entry.clone()).collect()
    }

    /// What round `round` reads about the one before it.
    pub fn prior_summary(&self, round: u32) -> Option<&RoundSummary> {
        round.checked_sub(1).and_then(|r| self.summaries.get(&r))
    }

    /// The participant's latest contribution before `round`.
    pub fn own_previous(&self, participant: &str, round: u32) -> Option<&Contribution> {
        self.contributions
            .iter()
            .filter(|c| c.participant == participant && c.round < round)
            .max_by_key(|c| c.round)
    }

    /// The round being worked on: the latest with a contribution, else 1.
    pub fn current_round(&self) -> u32 {
        self.contributions
            .iter()
            .map(|c| c.round)
            .max()
            .unwrap_or(1)
    }

    /// The newest unsettled call of kind `what` in `round`, with its handle:
    /// what a resumed host finds of a call a crash interrupted.
    pub fn open_call(&self, round: u32, what: &str) -> Option<(String, Option<String>)> {
        self.reservations
            .iter()
            .filter(|(_, r)| r.round == round && r.what == what && r.settled.is_none())
            .max_by_key(|(id, _)| id.get(1..).and_then(|n| n.parse::<u64>().ok()))
            .map(|(id, r)| (id.clone(), r.handle.clone()))
    }

    /// The round a call may be reserved for: the one being worked on.
    fn open_round(&self) -> Option<u32> {
        match self.next() {
            Next::Contribute { round, .. }
            | Next::Summarize { round, .. }
            | Next::Check { round }
            | Next::Decide { round } => Some(round),
            Next::NeedContract | Next::Finish { .. } | Next::Done => None,
        }
    }

    /// Rounds in a row, ending at the latest summarized one, whose digest
    /// added nothing.
    pub fn stalled_rounds(&self) -> u32 {
        let mut n = 0;
        for (_, s) in self.summaries.iter().rev() {
            if s.delta_empty() {
                n += 1;
            } else {
                break;
            }
        }
        n
    }

    fn gate_input(&self, round: u32) -> Option<GateInput<'_>> {
        let c = self.contract.as_ref()?;
        let (owner, pending) = self.flags.get(&round).copied().unwrap_or((false, false));
        Some(GateInput {
            round,
            max_rounds: c.max_rounds,
            tokens_used: self.tokens_used,
            budget: c.budget_tokens,
            verdicts: self.verdicts.get(&round).map(Vec::as_slice),
            stalled_rounds: self.stalled_rounds(),
            owner_only_disagreement: owner,
            pending_approval: pending,
        })
    }

    /// What must happen next.
    pub fn next(&self) -> Next {
        if self.closed.is_some() {
            return Next::Done;
        }
        if self.contract.is_none() {
            return Next::NeedContract;
        }
        // A stop recorded mid-round (a call that did not fit the budget)
        // ends the discussion wherever it was.
        if let Some((_, g)) = self.gates.iter().next_back() {
            if *g != Gate::Continue {
                return Next::Finish { outcome: *g };
            }
        }
        let round = self.current_round();
        let spoke: BTreeSet<&str> = self
            .contributions
            .iter()
            .filter(|c| c.round == round)
            .map(|c| c.participant.as_str())
            .collect();
        let missing: Vec<String> = self
            .roster(round)
            .into_iter()
            .filter(|p| !spoke.contains(p.as_str()))
            .collect();
        if !missing.is_empty() {
            return Next::Contribute { round, missing };
        }
        if !self.summaries.contains_key(&round) {
            let verbatim = is_small(&self.round_contributions(round));
            return Next::Summarize { round, verbatim };
        }
        if !self.verdicts.contains_key(&round) {
            return Next::Check { round };
        }
        match self.gates.get(&round) {
            None => Next::Decide { round },
            Some(Gate::Continue) => {
                let next = round + 1;
                Next::Contribute {
                    round: next,
                    missing: self.roster(next),
                }
            }
            Some(outcome) => Next::Finish { outcome: *outcome },
        }
    }
}

/// An open discussion log.
#[derive(Debug, Clone)]
pub struct DiscussionLog {
    path: PathBuf,
}

impl DiscussionLog {
    /// The log of discussion `id` in `dir`. The id must be a valid one.
    pub fn at(dir: &Path, id: &str) -> Result<Self, String> {
        if !crate::discussion::id_ok(id) {
            return Err(format!("invalid discussion id {id:?}"));
        }
        Ok(Self {
            path: dir.join(format!("{id}.jsonl")),
        })
    }

    /// `$CODECORA_HOME/tole/discussions`.
    pub fn default_dir() -> Option<PathBuf> {
        crate::paths::tole_data_dir().map(|d| d.join(DISCUSSIONS_DIR))
    }

    /// Hold the discussion's lock for one step: read the state, check the
    /// step, append. Two writers (two processes on one discussion) therefore
    /// never take the same sequence number or the same turn. The lock is a
    /// sidecar file, released when the guard drops.
    fn lock(&self) -> Result<std::fs::File, String> {
        if let Some(dir) = self.path.parent() {
            std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
        }
        let mut name = self.path.as_os_str().to_owned();
        name.push(".lock");
        let f = nofollow(
            std::fs::OpenOptions::new()
                .create(true)
                .truncate(false)
                .write(true),
        )
        .open(&name)
        .map_err(|e| format!("{}: {e}", std::path::Path::new(&name).display()))?;
        f.lock()
            .map_err(|e| format!("{}: {e}", std::path::Path::new(&name).display()))?;
        Ok(f)
    }

    fn events(&self) -> Result<Vec<Event>, String> {
        // Never through a symlink, never blocking on a FIFO, never past the
        // cap, checked on the opened file.
        let bytes = match crate::delegation_report::read_plain_file(&self.path, MAX_LOG_BYTES) {
            Ok(Some(b)) => b,
            Ok(None) => return Ok(Vec::new()),
            Err(e) => {
                return Err(format!(
                    "{}: {e} (the cap is {MAX_LOG_BYTES} bytes, over it is refused)",
                    self.path.display()
                ))
            }
        };
        // Bytes, then lossy: a crash can tear the last line inside a
        // multi-byte character, which must not make the whole file unreadable.
        let text = String::from_utf8_lossy(&bytes);
        let lines: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).collect();
        let mut out = Vec::new();
        for (i, l) in lines.iter().enumerate() {
            match serde_json::from_str::<Line>(l) {
                Ok(line) => out.push(line.event),
                // A crash mid-write leaves a torn last line, which was never
                // a step; anywhere else it is corruption.
                Err(_) if i + 1 == lines.len() && !text.ends_with('\n') => {}
                Err(e) => {
                    return Err(format!(
                        "{}: line {} is not a discussion event: {e}",
                        self.path.display(),
                        i + 1
                    ))
                }
            }
        }
        Ok(out)
    }

    /// The folded state.
    pub fn state(&self) -> Result<State, String> {
        Ok(State::fold(self.events()?))
    }

    fn append(&self, st: &State, event: Event, now: u64) -> Result<u64, String> {
        if let Some(dir) = self.path.parent() {
            std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
        }
        let seq = st.last_seq + 1;
        // Only the stop and the end may use the room kept for them.
        let ending = matches!(event, Event::Gate { .. } | Event::Closed { .. });
        let mut line =
            serde_json::to_string(&Line { seq, t: now, event }).map_err(|e| e.to_string())?;
        line.push('\n');
        if line.len() > MAX_RECORD_BYTES {
            return Err(format!(
                "a discussion record is {} bytes, over the {MAX_RECORD_BYTES}-byte cap; nothing was written",
                line.len()
            ));
        }
        let mut f = nofollow(
            std::fs::OpenOptions::new()
                .create(true)
                .read(true)
                .append(true),
        )
        .open(&self.path)
        .map_err(|e| format!("{}: {e}", self.path.display()))?;
        if !f.metadata().map_err(|e| e.to_string())?.is_file() {
            return Err(format!("{}: not a regular file", self.path.display()));
        }
        self.drop_torn_tail(&f)?;
        let len = f.metadata().map_err(|e| e.to_string())?.len();
        let room = if ending {
            MAX_LOG_BYTES
        } else {
            MAX_LOG_BYTES - CLOSE_RESERVE
        };
        if len.saturating_add(line.len() as u64) > room {
            return Err(format!(
                "{}: the discussion log is full ({len} of {MAX_LOG_BYTES} bytes); stop and close it",
                self.path.display()
            ));
        }
        f.write_all(line.as_bytes())
            .map_err(|e| format!("{}: {e}", self.path.display()))?;
        Ok(seq)
    }

    /// A crash mid-write leaves bytes after the last newline: a step that
    /// never completed (reads already ignore it). Cut them off before the
    /// next line is appended, or the new line would be glued onto the torn
    /// one and the log would no longer parse. Runs under the step's lock.
    fn drop_torn_tail(&self, f: &std::fs::File) -> Result<(), String> {
        use std::io::{Read as _, Seek as _, SeekFrom};
        let err = |e: std::io::Error| format!("{}: {e}", self.path.display());
        let len = f.metadata().map_err(err)?.len();
        if len == 0 {
            return Ok(());
        }
        let mut f = f;
        let mut last = [0u8; 1];
        f.seek(SeekFrom::Start(len - 1)).map_err(err)?;
        f.read_exact(&mut last).map_err(err)?;
        if last[0] == b'\n' {
            return Ok(());
        }
        // The log is capped, so reading it to find the last newline is cheap.
        f.seek(SeekFrom::Start(0)).map_err(err)?;
        let mut all = Vec::new();
        f.read_to_end(&mut all).map_err(err)?;
        let keep = all.iter().rposition(|b| *b == b'\n').map_or(0, |i| i + 1);
        f.set_len(keep as u64).map_err(err)
    }

    /// Reserve `tokens` for a provider call of kind `what` (a short id such
    /// as `participant-<name>`, `scribe` or `checker`) in the round being
    /// worked on, before the call runs. When it does not fit what is left of
    /// the budget, nothing is reserved and the budget escalation is recorded
    /// instead ([`Reserve::Stop`]).
    pub fn reserve(
        &self,
        round: u32,
        what: &str,
        tokens: u64,
        now: u64,
    ) -> Result<Reserve, String> {
        if !short_id(what, MAX_WHAT_CHARS, false) {
            return Err(format!(
                "call kind {what:?}: a short id of a-z, 0-9 and '-'"
            ));
        }
        let _guard = self.lock()?;
        let st = self.state()?;
        let next = st.next();
        if st.open_round() != Some(round) {
            return Err(Self::refuse(&next, &format!("a call in round {round}")));
        }
        let c = st.contract.as_ref().ok_or("no contract")?;
        if st.tokens_used.saturating_add(tokens) > c.budget_tokens {
            let outcome = Gate::Escalate(Reason::Budget);
            self.append(&st, Event::Gate { round, outcome }, now)?;
            return Ok(Reserve::Stop(outcome));
        }
        let call = format!("c{}", st.last_seq + 1);
        self.append(
            &st,
            Event::Reserve {
                round,
                call: call.clone(),
                what: what.to_string(),
                tokens,
            },
            now,
        )?;
        Ok(Reserve::Go(call))
    }

    /// Record the handle of a reserved call that runs elsewhere, once.
    pub fn bind(&self, call: &str, handle: &str, now: u64) -> Result<(), String> {
        if !short_id(handle, MAX_HANDLE_CHARS, true) {
            return Err(format!(
                "handle {handle:?}: a short id of letters, digits, '.', '_' and '-'"
            ));
        }
        let _guard = self.lock()?;
        let st = self.state()?;
        match st.reservations.get(call) {
            Some(r) if r.settled.is_none() && r.handle.is_none() => {}
            Some(_) => return Err(format!("call {call} is settled or already bound")),
            None => return Err(format!("no reserved call {call}")),
        }
        self.append(
            &st,
            Event::Bind {
                call: call.to_string(),
                handle: handle.to_string(),
            },
            now,
        )
        .map(|_| ())
    }

    /// Record what a reserved call spent, once; it replaces the estimate.
    pub fn settle(&self, call: &str, tokens: u64, now: u64) -> Result<(), String> {
        let _guard = self.lock()?;
        let st = self.state()?;
        if st.closed.is_some() {
            return Err("the discussion is closed".into());
        }
        match st.reservations.get(call) {
            Some(r) if r.settled.is_none() => {}
            Some(_) => return Err(format!("call {call} is already settled")),
            None => return Err(format!("no reserved call {call}")),
        }
        self.append(
            &st,
            Event::Settle {
                call: call.to_string(),
                tokens,
            },
            now,
        )
        .map(|_| ())
    }

    fn refuse(next: &Next, what: &str) -> String {
        format!("{what} is not the next step; the log expects {next:?}")
    }

    /// Record the contract. Once.
    pub fn start(&self, contract: Contract, now: u64) -> Result<(), String> {
        let _guard = self.lock()?;
        let st = self.state()?;
        if st.next() != Next::NeedContract {
            return Err("this discussion already has a contract".into());
        }
        self.append(&st, Event::Contract(contract), now).map(|_| ())
    }

    /// Change who speaks from `round` on (a subset of the contract's
    /// participants), before anyone has spoken in it.
    pub fn set_roster(&self, round: u32, participants: &[String], now: u64) -> Result<(), String> {
        let _guard = self.lock()?;
        let st = self.state()?;
        let next = st.next();
        let Some(c) = &st.contract else {
            return Err(Self::refuse(&next, "a roster"));
        };
        if st.closed.is_some() {
            return Err("the discussion is closed".into());
        }
        let ok_round = matches!(&next, Next::Contribute { round: r, missing }
            if *r == round && missing.len() == st.roster(round).len());
        if !ok_round {
            return Err(Self::refuse(&next, "a roster change"));
        }
        let known: BTreeSet<&str> = c.participants.iter().map(String::as_str).collect();
        if participants.is_empty() || participants.iter().any(|p| !known.contains(p.as_str())) {
            return Err("a roster is a non-empty subset of the contract's participants".into());
        }
        let unique: BTreeSet<&String> = participants.iter().collect();
        if unique.len() != participants.len() {
            return Err("a participant is listed twice".into());
        }
        self.append(
            &st,
            Event::Roster {
                round,
                participants: participants.to_vec(),
            },
            now,
        )
        .map(|_| ())
    }

    /// Record a participant's contribution (already sanitized). The log
    /// gives it its entry id, which every digest line must cite.
    pub fn contribute(
        &self,
        round: u32,
        participant: &str,
        text: &str,
        now: u64,
    ) -> Result<Contribution, String> {
        let _guard = self.lock()?;
        let st = self.state()?;
        let next = st.next();
        match &next {
            Next::Contribute { round: r, missing }
                if *r == round && missing.iter().any(|m| m == participant) => {}
            _ => {
                return Err(Self::refuse(
                    &next,
                    &format!("{participant}'s contribution to round {round}"),
                ))
            }
        }
        let c = Contribution {
            round,
            participant: participant.to_string(),
            entry: format!("e{}", st.last_seq + 1),
            text: text.to_string(),
        };
        self.append(&st, Event::Contribution(c.clone()), now)?;
        Ok(c)
    }

    /// Record the round's summary: the verbatim pass-through or a digest the
    /// caller has validated with `parse_digest`.
    pub fn summarize(&self, summary: RoundSummary, now: u64) -> Result<(), String> {
        let _guard = self.lock()?;
        let st = self.state()?;
        let next = st.next();
        let round = match &next {
            Next::Summarize { round, .. } if *round == summary.round() => *round,
            _ => {
                return Err(Self::refuse(
                    &next,
                    &format!("a summary of round {}", summary.round()),
                ))
            }
        };
        let known: BTreeSet<String> = st
            .round_contributions(round)
            .into_iter()
            .map(|c| c.entry)
            .collect();
        match &summary {
            RoundSummary::Verbatim { contributions, .. } => {
                let got: BTreeSet<String> = contributions.iter().map(|c| c.entry.clone()).collect();
                if got != known || !is_small(&st.round_contributions(round)) {
                    return Err("a verbatim summary holds exactly the round's contributions, and only a small round is passed on".into());
                }
            }
            RoundSummary::Fallback {
                contributions,
                reason,
                ..
            } => {
                let got: BTreeSet<String> = contributions.iter().map(|c| c.entry.clone()).collect();
                if got != known || is_small(&st.round_contributions(round)) {
                    return Err("a fallback holds exactly the round's contributions, and only a big round (one the scribe digests) has one".into());
                }
                if reason.chars().count() > MAX_FALLBACK_REASON_CHARS {
                    return Err(format!(
                        "a fallback's reason is over {MAX_FALLBACK_REASON_CHARS} characters"
                    ));
                }
            }
            RoundSummary::Digest(d) => {
                let cited = d
                    .decisions
                    .iter()
                    .chain(&d.claims)
                    .chain(&d.actions)
                    .chain(d.disagreements.iter().flat_map(|x| x.positions.iter()));
                if let Some(bad) = cited.map(|i| &i.entry).find(|e| !known.contains(*e)) {
                    return Err(format!(
                        "the digest cites {bad:?}, not an entry of round {round}"
                    ));
                }
            }
        }
        self.append(&st, Event::Summary(summary), now).map(|_| ())
    }

    /// Record the claim-checker's verdicts for the round just summarized.
    pub fn check(
        &self,
        round: u32,
        verdicts: Vec<CriterionVerdict>,
        now: u64,
    ) -> Result<(), String> {
        let _guard = self.lock()?;
        let st = self.state()?;
        let next = st.next();
        if next != (Next::Check { round }) {
            return Err(Self::refuse(&next, &format!("verdicts for round {round}")));
        }
        let c = st.contract.as_ref().ok_or("no contract")?;
        if verdicts.len() != c.criteria.len() {
            return Err(format!(
                "{} verdicts for {} criteria",
                verdicts.len(),
                c.criteria.len()
            ));
        }
        let known = st.known_entries();
        if verdicts
            .iter()
            .any(|v| v.evidence.iter().any(|e| !known.contains(e)))
        {
            return Err("a verdict cites an entry that is not in the discussion".into());
        }
        if verdicts.iter().any(|v| v.met && v.evidence.is_empty()) {
            return Err("a criterion called met has no evidence".into());
        }
        self.append(&st, Event::Verdicts { round, verdicts }, now)
            .map(|_| ())
    }

    /// Add tokens spent by any role.
    pub fn usage(&self, round: u32, tokens: u64, now: u64) -> Result<(), String> {
        let _guard = self.lock()?;
        let st = self.state()?;
        if st.closed.is_some() {
            return Err("the discussion is closed".into());
        }
        self.append(&st, Event::Usage { round, tokens }, now)
            .map(|_| ())
    }

    /// Record what only the host can see after `round`.
    pub fn flags(
        &self,
        round: u32,
        owner_only: bool,
        pending_approval: bool,
        now: u64,
    ) -> Result<(), String> {
        let _guard = self.lock()?;
        let st = self.state()?;
        if st.closed.is_some() || st.gates.contains_key(&round) {
            return Err("the gate for this round is already decided".into());
        }
        self.append(
            &st,
            Event::Flags {
                round,
                owner_only_disagreement: owner_only,
                pending_approval,
            },
            now,
        )
        .map(|_| ())
    }

    /// Compute the gate from the ledger and record it. The caller does not
    /// supply the outcome.
    pub fn decide(&self, round: u32, now: u64) -> Result<Gate, String> {
        let _guard = self.lock()?;
        let st = self.state()?;
        let next = st.next();
        if next != (Next::Decide { round }) {
            return Err(Self::refuse(&next, &format!("the gate for round {round}")));
        }
        let outcome = gate(&st.gate_input(round).ok_or("no contract")?);
        self.append(&st, Event::Gate { round, outcome }, now)?;
        Ok(outcome)
    }

    /// Stop the discussion wherever it is: the stop is recorded as the
    /// gate's escalation ([`Reason::Stopped`]), so the driver closes it with
    /// the usual report and nothing in a participant's text can start or
    /// prevent it. A discussion that is already ending or closed keeps its
    /// outcome. Returns the outcome.
    pub fn stop(&self, now: u64) -> Result<Gate, String> {
        let _guard = self.lock()?;
        let st = self.state()?;
        match st.next() {
            Next::NeedContract => Err("the discussion has no contract".into()),
            Next::Finish { outcome } => Ok(outcome),
            Next::Done => Ok(st.closed.map_or(Gate::Continue, |(g, _)| g)),
            _ => {
                let round = st.open_round().ok_or("no round is open")?;
                let outcome = Gate::Escalate(Reason::Stopped);
                self.append(&st, Event::Gate { round, outcome }, now)?;
                Ok(outcome)
            }
        }
    }

    /// Close the discussion with the text for the owner, after the gate said
    /// report or escalate.
    pub fn close(&self, text: &str, now: u64) -> Result<Gate, String> {
        let _guard = self.lock()?;
        let st = self.state()?;
        let next = st.next();
        let Next::Finish { outcome } = next else {
            return Err(Self::refuse(&next, "closing"));
        };
        let text = if text.chars().count() > MAX_CLOSE_CHARS {
            let mut t: String = text.chars().take(MAX_CLOSE_CHARS - 1).collect();
            t.push('\u{2026}');
            t
        } else {
            text.to_string()
        };
        self.append(&st, Event::Closed { outcome, text }, now)?;
        Ok(outcome)
    }
}

/// Opens never follow a symlink at the last component (the log and its
/// lock are host files, but a refused symlink costs nothing).
fn nofollow(o: &mut std::fs::OpenOptions) -> &mut std::fs::OpenOptions {
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        o.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    o
}

/// A call kind (`a-z 0-9 -`) or, with `handle`, a handle (letters, digits,
/// `.`, `_`, `-`): short, never starting with `-` or `.`.
fn short_id(s: &str, max: usize, handle: bool) -> bool {
    !s.is_empty()
        && s.len() <= max
        && !s.starts_with(['-', '.'])
        && s.bytes().all(|b| {
            b.is_ascii_lowercase()
                || b.is_ascii_digit()
                || b == b'-'
                || (handle && (b.is_ascii_uppercase() || b == b'.' || b == b'_'))
        })
}

/// Why a closed discussion was handed back, for display.
pub fn describe(outcome: Gate) -> &'static str {
    match outcome {
        Gate::Report => "the acceptance criteria are met",
        Gate::Escalate(Reason::PendingApproval) => "a Write is waiting for your approval",
        Gate::Escalate(Reason::OwnerDecision) => "a disagreement only you can decide",
        Gate::Escalate(Reason::Budget) => "the token budget is spent",
        Gate::Escalate(Reason::MaxRounds) => "the round limit is reached",
        Gate::Escalate(Reason::Stalled) => "two rounds in a row added nothing new",
        Gate::Escalate(Reason::Stopped) => "it was stopped before it finished",
        Gate::Continue => "the discussion continues",
    }
}

#[cfg(test)]
#[path = "log_tests.rs"]
mod tests;
