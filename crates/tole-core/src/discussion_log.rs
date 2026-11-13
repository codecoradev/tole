//! The discussion ledger (#588): every step of a discussion is one line in
//! an append-only JSONL file, and the state, including what comes next, is
//! folded from it. A crash mid-discussion resumes from the file; a line is
//! never rewritten.
//!
//! The log is also the state machine's referee. Each step is checked against
//! [`Next`], what the log says must happen now: a contribution only from a
//! participant who has not spoken this round, a summary only once everyone has,
//! the checker's verdicts only after the summary, and the gate's result is
//! computed here from the ledger rather than accepted from the caller, so no
//! step can be skipped, repeated or forged by the code driving the
//! discussion.

use crate::discussion::{
    gate, is_small, Contract, Contribution, CriterionVerdict, Gate, GateInput, Reason, RoundSummary,
};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::io::Write as _;
use std::path::{Path, PathBuf};

/// Largest log read back.
pub const MAX_LOG_BYTES: u64 = 16 * 1024 * 1024;
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

/// The folded log.
#[derive(Debug, Clone, Default)]
pub struct State {
    pub contract: Option<Contract>,
    rosters: BTreeMap<u32, Vec<String>>,
    pub contributions: Vec<Contribution>,
    pub summaries: BTreeMap<u32, RoundSummary>,
    pub verdicts: BTreeMap<u32, Vec<CriterionVerdict>>,
    pub tokens_used: u64,
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
        let f = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&name)
            .map_err(|e| format!("{}: {e}", std::path::Path::new(&name).display()))?;
        f.lock()
            .map_err(|e| format!("{}: {e}", std::path::Path::new(&name).display()))?;
        Ok(f)
    }

    fn events(&self) -> Result<Vec<Event>, String> {
        let meta = match std::fs::metadata(&self.path) {
            Ok(m) => m,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(format!("{}: {e}", self.path.display())),
        };
        if meta.len() > MAX_LOG_BYTES {
            return Err(format!(
                "{}: over {MAX_LOG_BYTES} bytes",
                self.path.display()
            ));
        }
        let text = std::fs::read_to_string(&self.path)
            .map_err(|e| format!("{}: {e}", self.path.display()))?;
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
        let mut line =
            serde_json::to_string(&Line { seq, t: now, event }).map_err(|e| e.to_string())?;
        line.push('\n');
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
            .map_err(|e| format!("{}: {e}", self.path.display()))?;
        f.write_all(line.as_bytes())
            .map_err(|e| format!("{}: {e}", self.path.display()))?;
        Ok(seq)
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

    /// Close the discussion with the text for the owner, after the gate said
    /// report or escalate.
    pub fn close(&self, text: &str, now: u64) -> Result<Gate, String> {
        let _guard = self.lock()?;
        let st = self.state()?;
        let next = st.next();
        let Next::Finish { outcome } = next else {
            return Err(Self::refuse(&next, "closing"));
        };
        self.append(
            &st,
            Event::Closed {
                outcome,
                text: text.to_string(),
            },
            now,
        )?;
        Ok(outcome)
    }
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
        Gate::Continue => "the discussion continues",
    }
}

#[cfg(test)]
#[path = "discussion_log_tests.rs"]
mod tests;
