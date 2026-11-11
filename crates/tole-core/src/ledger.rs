//! Append-only automation ledger: claim, fold and recovery (issue #578,
//! design `docs/automation-design.md` §2–4).
//!
//! The ledger is one JSONL log of job definitions and run records; current
//! state is a pure fold over it ([`LedgerState`]). Nothing here reads a
//! clock, spawns, sleeps or locks: `now` is passed in, process liveness is
//! an injected predicate and bytes go through a [`LedgerStore`].
//!
//! # Guarantees (the Hermes failure classes, made structural)
//!
//! - **At most once.** A run starts only after its `fire` record (the
//!   claim) is appended. The first `fire` for a `(job_id, scheduled_for)`
//!   in log order owns it; [`Ledger::claim`] refuses a claimed occurrence
//!   and re-reads after appending, so even two ticks that race past the
//!   host lock cannot both win (#105861 follow-up, P3).
//! - **A terminal record is never overwritten.** The first `done` for a
//!   `fire_id` is final; [`Ledger::settle`] refuses a second, and the fold
//!   ignores later ones. Delivery is a separate record that cannot touch
//!   `done.status` (#105861, P2).
//! - **No silent skip.** Every owed occurrence that does not run becomes a
//!   `missed` record with a machine-readable reason (#9086, P1).
//! - **No heartbeats, no lock timeouts.** Ownership is a `{pid, start_time}`
//!   lease checked only by [`Ledger::recover`], which turns a `fire` without
//!   `done` whose lease is dead into an `interrupted` `done`.
//!
//! # Host contract
//!
//! The host (#579) must hold an exclusive lock (e.g. `flock`) around every
//! **mutating** call (`put_job`, `claim`, `block`, `settle`, `deliver`,
//! `recover`, `record_hook`) — milliseconds, never around execution. Under
//! that lock a final line without a newline can only come from a crashed
//! writer, so mutating calls truncate it (as `storage.rs` does on open)
//! before appending. Read-only calls ([`Ledger::open`], [`Ledger::refresh`])
//! never truncate. Appends must be whole lines (`O_APPEND` + fsync).
//! The file adapter is left to #579: the session storage helpers are
//! session-file specific (header, seq), so reusing them would mean
//! duplicating them.
//!
//! # Format
//!
//! One JSON object per line, `{"v":1,"kind":"fire",...}`. Times are Unix
//! seconds (UTC). A later `job` with the same `id` supersedes; pause and
//! removal are new `job` records. A record of an unknown `kind` is skipped
//! (forward compatibility); a known kind with a newer `v` is a hard error
//! (refuse to guess). A torn or unparseable final line is discarded.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::cron::Schedule;

/// Version of the ledger record format.
pub const LEDGER_VERSION: u32 = 1;
/// How late (seconds) an occurrence may start and still be on time.
pub const ON_TIME_SECS: i64 = 60;
/// Upper bound for `grace_minutes` (one week).
pub const MAX_GRACE_MINUTES: u32 = 7 * 24 * 60;

/// Field names a job's host-owned `extra` map must not use.
const RESERVED: &[&str] = &[
    "v",
    "kind",
    "id",
    "schedule",
    "timezone",
    "catch_up",
    "grace_minutes",
    "paused",
    "removed",
    "at",
];

/// Errors from the ledger.
#[derive(Debug, thiserror::Error)]
pub enum LedgerError {
    /// The store failed (I/O, injected fault).
    #[error("ledger store: {0}")]
    Store(String),
    /// A non-final line does not parse.
    #[error("corrupt ledger at byte {offset}: {reason}")]
    Corrupt { offset: u64, reason: String },
    /// A known record kind was written by a newer format version.
    #[error("ledger record `{kind}` has version {found}, newer than {LEDGER_VERSION}; refusing to guess")]
    NewerVersion { kind: String, found: u64 },
    /// A job failed validation on save.
    #[error("invalid job: {0}")]
    InvalidJob(String),
    #[error("unknown job `{0}`")]
    UnknownJob(String),
    #[error("unknown fire `{0}`")]
    UnknownFire(String),
    /// The job is paused or removed.
    #[error("job `{0}` is paused or removed")]
    JobInactive(String),
    /// The occurrence already has a `fire`.
    #[error("`{job_id}` at {scheduled_for} is already claimed by fire `{fire_id}`")]
    AlreadyClaimed {
        job_id: String,
        scheduled_for: i64,
        fire_id: String,
    },
    /// The occurrence is already accounted (fired, missed or blocked).
    #[error("`{job_id}` at {scheduled_for} is already accounted")]
    AlreadyAccounted { job_id: String, scheduled_for: i64 },
    /// Another writer's record landed first in the log.
    #[error("lost the race: `{winner}` was appended first")]
    LostRace { winner: String },
    /// The run already has a terminal record.
    #[error(
        "fire `{fire_id}` is already settled as {status:?}; a done record is never overwritten"
    )]
    AlreadySettled { fire_id: String, status: RunStatus },
    /// Delivery before the run settled.
    #[error("fire `{0}` has no done record yet")]
    NotSettled(String),
}

/// Catch-up policy for occurrences that could not start on time.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CatchUp {
    /// Only an on-time occurrence runs; late ones become `missed`.
    #[default]
    Skip,
    /// One late run of the most recent occurrence, within `grace_minutes`.
    One,
}

/// A job definition. Later records with the same `id` supersede.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Job {
    pub id: String,
    /// Cron or `@every` text (validated on save).
    pub schedule: String,
    /// Absent = UTC. Only `UTC` is accepted until #580.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timezone: Option<String>,
    #[serde(default)]
    pub catch_up: CatchUp,
    #[serde(default)]
    pub grace_minutes: u32,
    #[serde(default)]
    pub paused: bool,
    #[serde(default)]
    pub removed: bool,
    /// When this version takes effect: only occurrences strictly after it
    /// are owed, and `@every` periods are counted from it.
    pub at: i64,
    /// Host-owned fields (name, prompt, mode, allow, budget, …; #579).
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

/// Process lease of a claim: identity of the claiming process.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Lease {
    pub pid: u32,
    /// Process start time as reported by the host (opaque; guards pid reuse).
    pub start_time: u64,
}

/// The at-most-once claim, appended before execution.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Fire {
    pub fire_id: String,
    pub job_id: String,
    pub scheduled_for: i64,
    pub claimed_at: i64,
    pub lease: Lease,
}

/// Outcome of a run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunStatus {
    Ok,
    Error,
    Cancelled,
    Budget,
    AwaitingApproval,
    Interrupted,
}

/// Terminal record of a run. Never overwritten.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Done {
    pub fire_id: String,
    pub job_id: String,
    pub status: RunStatus,
    /// The run's session (its transcript), when one was created.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<Value>,
    pub at: i64,
}

/// Outcome of delivering a run's result.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeliveryStatus {
    Sent,
    Failed,
    Skipped,
}

/// A delivery attempt; separate from the run status.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Delivery {
    pub fire_id: String,
    pub target: String,
    pub attempt: u32,
    pub status: DeliveryStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    pub at: i64,
}

/// Why owed occurrences did not run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MissReason {
    /// Later than the catch-up policy allows.
    Late,
    /// A more recent occurrence ran instead (runs never stack).
    Superseded,
}

/// A contiguous range of owed occurrences that did not run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Missed {
    pub job_id: String,
    /// First skipped occurrence.
    pub scheduled_for: i64,
    /// Last skipped occurrence.
    pub last: i64,
    pub count: u64,
    pub reason: MissReason,
    pub at: i64,
}

/// An occurrence refused by pre-dispatch validation (P10, zero tokens).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Blocked {
    pub job_id: String,
    pub scheduled_for: i64,
    /// Machine-readable code, `[a-z0-9_]+` (e.g. `skill_missing`).
    pub reason: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    pub at: i64,
}

/// Durable webhook idempotency marker.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HookSeen {
    pub route: String,
    pub delivery_id: String,
    pub at: i64,
}

/// One ledger line.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Record {
    Job(Job),
    Fire(Fire),
    Done(Done),
    Delivery(Delivery),
    Missed(Missed),
    Blocked(Blocked),
    HookSeen(HookSeen),
}

const KINDS: &[&str] = &[
    "job",
    "fire",
    "done",
    "delivery",
    "missed",
    "blocked",
    "hook_seen",
];

impl Record {
    /// Serializes as one line (with `v`, without the newline).
    pub fn to_line(&self) -> String {
        let mut v = serde_json::to_value(self).expect("ledger records always serialize");
        if let Value::Object(m) = &mut v {
            m.insert("v".into(), LEDGER_VERSION.into());
        }
        v.to_string()
    }

    /// Parses one line. `Ok(None)` = a kind this build does not know.
    pub fn parse_line(line: &str) -> Result<Option<Record>, LedgerError> {
        #[derive(Deserialize)]
        struct Head {
            kind: String,
            #[serde(default)]
            v: u64,
        }
        let bad = |e: serde_json::Error| LedgerError::Corrupt {
            offset: 0,
            reason: e.to_string(),
        };
        // Two typed passes beat one `Value` pass plus a conversion (#578 perf).
        let head: Head = serde_json::from_str(line).map_err(bad)?;
        if !KINDS.contains(&head.kind.as_str()) {
            return Ok(None);
        }
        if head.v > u64::from(LEDGER_VERSION) {
            return Err(LedgerError::NewerVersion {
                kind: head.kind,
                found: head.v,
            });
        }
        let mut rec: Record = serde_json::from_str(line).map_err(bad)?;
        if let Record::Job(j) = &mut rec {
            j.extra.remove("v"); // the flattened map also captures `v`
        }
        Ok(Some(rec))
    }
}

/// Byte log the ledger appends to. See the module docs for the contract.
pub trait LedgerStore {
    /// Bytes from `offset` to the current end.
    fn read_from(&mut self, offset: u64) -> Result<Vec<u8>, LedgerError>;
    /// Appends one complete line (ending in `\n`), durably.
    fn append(&mut self, line: &[u8]) -> Result<(), LedgerError>;
    /// Drops everything past `len` (torn-tail repair only).
    fn truncate(&mut self, len: u64) -> Result<(), LedgerError>;
}

/// In-memory store; clones share one buffer (two "processes" in tests).
#[derive(Debug, Clone, Default)]
pub struct MemoryStore(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);

impl MemoryStore {
    /// A copy of the current bytes.
    pub fn bytes(&self) -> Vec<u8> {
        self.0.lock().map(|b| b.clone()).unwrap_or_default()
    }
    fn buf(&self) -> Result<std::sync::MutexGuard<'_, Vec<u8>>, LedgerError> {
        self.0
            .lock()
            .map_err(|_| LedgerError::Store("poisoned".into()))
    }
}

impl LedgerStore for MemoryStore {
    fn read_from(&mut self, offset: u64) -> Result<Vec<u8>, LedgerError> {
        let b = self.buf()?;
        Ok(b.get(offset as usize..).unwrap_or_default().to_vec())
    }
    fn append(&mut self, line: &[u8]) -> Result<(), LedgerError> {
        self.buf()?.extend_from_slice(line);
        Ok(())
    }
    fn truncate(&mut self, len: u64) -> Result<(), LedgerError> {
        self.buf()?.truncate(len as usize);
        Ok(())
    }
}

/// A job as folded: its latest record, parsed schedule and cursor.
#[derive(Debug, Clone)]
pub struct JobState {
    pub job: Job,
    /// `Err` only for a hand-edited or newer-build record (save validates).
    pub schedule: Result<Schedule, String>,
}

/// Current state: a pure fold over the records.
#[derive(Debug, Clone, Default)]
pub struct LedgerState {
    jobs: BTreeMap<String, JobState>,
    fires: HashMap<String, Fire>,
    claims: HashMap<(String, i64), String>,
    done: HashMap<String, Done>,
    pending: BTreeMap<String, BTreeSet<String>>,
    accounted: HashMap<String, i64>,
    last_done: HashMap<String, (i64, String)>,
    deliveries: BTreeMap<(String, String, u32), Delivery>,
    missed: BTreeMap<(String, i64), Missed>,
    blocked: BTreeMap<(String, i64), Blocked>,
    hooks: HashMap<(String, String), i64>,
    /// Records of kinds this build does not know (skipped).
    pub unknown: u64,
}

/// What [`Ledger::ready`] says to do for one job.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Due {
    pub job_id: String,
    /// The occurrence to run, if any.
    pub fire: Option<i64>,
    /// Owed occurrences that will not run (always recorded).
    pub missed: Option<Missed>,
}

impl LedgerState {
    /// Folds records in log order.
    pub fn fold<'a>(records: impl IntoIterator<Item = &'a Record>) -> Self {
        let mut s = Self::default();
        records.into_iter().for_each(|r| s.apply(r));
        s
    }

    /// Applies one record. Idempotent per record identity; first `fire`
    /// per occurrence and first `done` per fire win.
    pub fn apply(&mut self, rec: &Record) {
        match rec {
            Record::Job(j) => {
                let schedule = validate_timezone(j)
                    .and_then(|_| Schedule::parse(&j.schedule).map_err(|e| e.to_string()));
                self.jobs.insert(
                    j.id.clone(),
                    JobState {
                        job: j.clone(),
                        schedule,
                    },
                );
            }
            Record::Fire(f) => {
                let key = (f.job_id.clone(), f.scheduled_for);
                if self.claims.contains_key(&key) || self.fires.contains_key(&f.fire_id) {
                    return;
                }
                self.claims.insert(key, f.fire_id.clone());
                self.fires.insert(f.fire_id.clone(), f.clone());
                self.account(&f.job_id, f.scheduled_for);
                match self.done.contains_key(&f.fire_id) {
                    true => self.note_done(f, f.fire_id.clone()),
                    false => {
                        self.pending
                            .entry(f.job_id.clone())
                            .or_default()
                            .insert(f.fire_id.clone());
                    }
                }
            }
            Record::Done(d) => {
                if self.done.contains_key(&d.fire_id) {
                    return; // never overwritten
                }
                self.done.insert(d.fire_id.clone(), d.clone());
                if let Some(f) = self.fires.get(&d.fire_id).cloned() {
                    if let Some(p) = self.pending.get_mut(&f.job_id) {
                        p.remove(&f.fire_id);
                    }
                    self.note_done(&f, d.fire_id.clone());
                }
            }
            Record::Delivery(d) => {
                let key = (d.fire_id.clone(), d.target.clone(), d.attempt);
                self.deliveries.entry(key).or_insert_with(|| d.clone());
            }
            Record::Missed(m) => {
                self.account(&m.job_id, m.last);
                let key = (m.job_id.clone(), m.scheduled_for);
                self.missed.entry(key).or_insert_with(|| m.clone());
            }
            Record::Blocked(b) => {
                self.account(&b.job_id, b.scheduled_for);
                let key = (b.job_id.clone(), b.scheduled_for);
                self.blocked.entry(key).or_insert_with(|| b.clone());
            }
            Record::HookSeen(h) => {
                let key = (h.route.clone(), h.delivery_id.clone());
                self.hooks.entry(key).or_insert(h.at);
            }
        }
    }

    fn account(&mut self, job: &str, t: i64) {
        let c = self.accounted.entry(job.to_string()).or_insert(t);
        *c = (*c).max(t);
    }

    fn note_done(&mut self, f: &Fire, fire_id: String) {
        let e = self
            .last_done
            .entry(f.job_id.clone())
            .or_insert((f.scheduled_for, fire_id.clone()));
        if f.scheduled_for > e.0 {
            *e = (f.scheduled_for, fire_id);
        }
    }

    pub fn job(&self, id: &str) -> Option<&JobState> {
        self.jobs.get(id)
    }
    pub fn jobs(&self) -> impl Iterator<Item = &JobState> {
        self.jobs.values()
    }
    pub fn fire(&self, fire_id: &str) -> Option<&Fire> {
        self.fires.get(fire_id)
    }
    /// The winning `fire_id` for an occurrence.
    pub fn claim_of(&self, job_id: &str, scheduled_for: i64) -> Option<&str> {
        self.claims
            .get(&(job_id.to_string(), scheduled_for))
            .map(String::as_str)
    }
    pub fn done(&self, fire_id: &str) -> Option<&Done> {
        self.done.get(fire_id)
    }
    /// Fires without a `done`, in `(job_id, fire_id)` order.
    pub fn pending(&self) -> impl Iterator<Item = &Fire> {
        self.pending.values().flatten().map(|id| &self.fires[id])
    }
    /// Outcome of the job's latest settled occurrence.
    pub fn last_outcome(&self, job_id: &str) -> Option<&Done> {
        self.last_done.get(job_id).map(|(_, id)| &self.done[id])
    }
    /// Latest accounted (fired, missed or blocked) occurrence.
    pub fn accounted(&self, job_id: &str) -> Option<i64> {
        self.accounted.get(job_id).copied()
    }
    /// Delivery attempts of one run, by `(target, attempt)`.
    pub fn deliveries<'a>(&'a self, fire_id: &'a str) -> impl Iterator<Item = &'a Delivery> + 'a {
        self.deliveries
            .range((fire_id.to_string(), String::new(), 0)..)
            .map(|(_, d)| d)
            .take_while(move |d| d.fire_id == fire_id)
    }
    pub fn missed(&self) -> impl Iterator<Item = &Missed> {
        self.missed.values()
    }
    pub fn blocked(&self) -> impl Iterator<Item = &Blocked> {
        self.blocked.values()
    }
    pub fn hook_seen(&self, route: &str, delivery_id: &str) -> bool {
        self.hooks
            .contains_key(&(route.to_string(), delivery_id.to_string()))
    }

    /// Next occurrence the job is owed after everything accounted.
    pub fn next_run(&self, job_id: &str) -> Option<i64> {
        let js = self.jobs.get(job_id)?;
        let s = js.schedule.as_ref().ok()?;
        if js.job.paused || js.job.removed {
            return None;
        }
        s.next_after(self.cursor(&js.job), js.job.at)
    }

    fn cursor(&self, job: &Job) -> i64 {
        let floor = job.at;
        self.accounted
            .get(&job.id)
            .map_or(floor, |a| (*a).max(floor))
    }

    /// What is due at `now` (pure). Jobs that are paused, removed, invalid
    /// or still running are not listed; their owed occurrences wait.
    pub fn ready(&self, now: i64) -> Vec<Due> {
        let mut out = Vec::new();
        for js in self.jobs.values() {
            let job = &js.job;
            let Ok(s) = &js.schedule else { continue };
            if job.paused || job.removed || self.pending.get(&job.id).is_some_and(|p| !p.is_empty())
            {
                continue;
            }
            let Some(span) = s.span(self.cursor(job), now, job.at) else {
                continue;
            };
            let grace = match job.catch_up {
                CatchUp::Skip => ON_TIME_SECS,
                CatchUp::One => ON_TIME_SECS.max(i64::from(job.grace_minutes) * 60),
            };
            let missed = |first, last, count, reason| Missed {
                job_id: job.id.clone(),
                scheduled_for: first,
                last,
                count,
                reason,
                at: now,
            };
            let due = if now - span.last <= grace {
                Due {
                    job_id: job.id.clone(),
                    fire: Some(span.last),
                    missed: span
                        .before_last
                        .map(|b| missed(span.first, b, span.count - 1, MissReason::Superseded)),
                }
            } else {
                Due {
                    job_id: job.id.clone(),
                    fire: None,
                    missed: Some(missed(span.first, span.last, span.count, MissReason::Late)),
                }
            };
            out.push(due);
        }
        out
    }
}

fn validate_timezone(j: &Job) -> Result<(), String> {
    match j.timezone.as_deref() {
        None | Some("UTC") => Ok(()),
        Some(z) => Err(format!(
            "time zone `{z}` is not supported yet (UTC only until #580)"
        )),
    }
}

fn valid_code(s: &str, max: usize) -> bool {
    !s.is_empty() && s.len() <= max
}

/// The ledger: a store plus the fold of everything read from it.
#[derive(Debug)]
pub struct Ledger<S: LedgerStore> {
    store: S,
    state: LedgerState,
    offset: u64,
}

impl<S: LedgerStore> Ledger<S> {
    /// Reads and folds the log (read-only: never truncates).
    pub fn open(store: S) -> Result<Self, LedgerError> {
        let mut l = Ledger {
            store,
            state: LedgerState::default(),
            offset: 0,
        };
        l.sync(false)?;
        Ok(l)
    }

    /// The folded state.
    pub fn state(&self) -> &LedgerState {
        &self.state
    }

    /// The store (e.g. to hand a test's shared buffer around).
    pub fn store(&self) -> &S {
        &self.store
    }

    /// Folds records appended since the last read (read-only).
    pub fn refresh(&mut self) -> Result<(), LedgerError> {
        self.sync(false)
    }

    /// Incremental read + fold. With `repair`, a torn final line is
    /// truncated (callers hold the host lock, see the module docs).
    fn sync(&mut self, repair: bool) -> Result<(), LedgerError> {
        let bytes = self.store.read_from(self.offset)?;
        let mut pos = 0usize;
        while pos < bytes.len() {
            let Some(nl) = bytes[pos..].iter().position(|b| *b == b'\n') else {
                break; // torn tail (no newline)
            };
            let end = pos + nl + 1;
            let parsed = match std::str::from_utf8(&bytes[pos..end - 1]) {
                Ok(l) if l.trim().is_empty() => Ok(Some(None)),
                Ok(l) => Record::parse_line(l.trim()).map(|r| r.map(Some)),
                Err(e) => Err(LedgerError::Corrupt {
                    offset: 0,
                    reason: e.to_string(),
                }),
            };
            match parsed {
                Ok(Some(Some(rec))) => self.state.apply(&rec),
                Ok(Some(None)) => {} // blank line
                Ok(None) => self.state.unknown += 1,
                Err(LedgerError::Corrupt { .. }) if end == bytes.len() => break, // torn
                Err(e) => {
                    // Keep the folded prefix consistent with the offset.
                    let at = self.offset + pos as u64;
                    self.offset = at;
                    return Err(match e {
                        LedgerError::Corrupt { reason, .. } => {
                            LedgerError::Corrupt { offset: at, reason }
                        }
                        other => other,
                    });
                }
            }
            pos = end;
        }
        self.offset += pos as u64;
        if repair && pos < bytes.len() {
            self.store.truncate(self.offset)?;
        }
        Ok(())
    }

    /// Appends one record and folds it (and anything others appended).
    fn append(&mut self, rec: &Record) -> Result<(), LedgerError> {
        self.sync(true)?;
        let mut line = rec.to_line().into_bytes();
        line.push(b'\n');
        self.store.append(&line)?;
        self.sync(true)
    }

    /// Validates and appends a job definition (also used to pause, resume
    /// and remove: write the job again with the changed fields).
    pub fn put_job(&mut self, job: Job) -> Result<(), LedgerError> {
        let bad = |m: String| Err(LedgerError::InvalidJob(m));
        if job.id.is_empty()
            || job.id.len() > 64
            || !job
                .id
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
        {
            return bad(format!("id `{}` must be 1-64 of [A-Za-z0-9._-]", job.id));
        }
        Schedule::parse(&job.schedule).map_err(|e| LedgerError::InvalidJob(e.to_string()))?;
        validate_timezone(&job).map_err(LedgerError::InvalidJob)?;
        if job.grace_minutes > MAX_GRACE_MINUTES {
            return bad(format!(
                "grace_minutes {} exceeds {MAX_GRACE_MINUTES}",
                job.grace_minutes
            ));
        }
        if let Some(k) = job.extra.keys().find(|k| RESERVED.contains(&k.as_str())) {
            return bad(format!("host field `{k}` collides with a ledger field"));
        }
        self.append(&Record::Job(job))
    }

    /// What is due at `now`, from the state as last read. Call under the
    /// host lock and follow with [`claim`](Self::claim) or
    /// [`block`](Self::block) for each entry.
    pub fn ready(&mut self, now: i64) -> Result<Vec<Due>, LedgerError> {
        self.sync(true)?;
        Ok(self.state.ready(now))
    }

    /// Records `due.missed`, then claims `due.fire` with `lease`. Returns
    /// the winning `fire` to execute, or `None` when nothing runs.
    pub fn claim(
        &mut self,
        due: &Due,
        lease: Lease,
        now: i64,
    ) -> Result<Option<Fire>, LedgerError> {
        self.record_missed(due)?;
        let Some(at) = due.fire else { return Ok(None) };
        self.check_claimable(&due.job_id, at)?;
        let fire_id = format!("{}@{at}/{}.{}", due.job_id, lease.pid, lease.start_time);
        self.append(&Record::Fire(Fire {
            fire_id: fire_id.clone(),
            job_id: due.job_id.clone(),
            scheduled_for: at,
            claimed_at: now,
            lease,
        }))?;
        match self.state.claim_of(&due.job_id, at) {
            Some(w) if w == fire_id => Ok(self.state.fire(&fire_id).cloned()),
            w => Err(LedgerError::LostRace {
                winner: w.unwrap_or_default().to_string(),
            }),
        }
    }

    /// Records `due.missed`, then a `blocked` record instead of a fire
    /// (pre-dispatch validation failed; nothing runs, zero tokens).
    pub fn block(
        &mut self,
        due: &Due,
        reason: &str,
        detail: Option<String>,
        now: i64,
    ) -> Result<(), LedgerError> {
        if !valid_code(reason, 64)
            || !reason
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
        {
            return Err(LedgerError::InvalidJob(format!(
                "blocked reason `{reason}` must be 1-64 of [a-z0-9_]"
            )));
        }
        self.record_missed(due)?;
        let Some(at) = due.fire else { return Ok(()) };
        self.check_claimable(&due.job_id, at)?;
        self.append(&Record::Blocked(Blocked {
            job_id: due.job_id.clone(),
            scheduled_for: at,
            reason: reason.to_string(),
            detail,
            at: now,
        }))
    }

    fn record_missed(&mut self, due: &Due) -> Result<(), LedgerError> {
        self.sync(true)?;
        let Some(m) = &due.missed else { return Ok(()) };
        if self.state.accounted(&m.job_id).is_some_and(|a| a >= m.last) {
            return Ok(()); // already recorded (by us earlier or by a racer)
        }
        self.append(&Record::Missed(m.clone()))
    }

    fn check_claimable(&self, job_id: &str, at: i64) -> Result<(), LedgerError> {
        let js = self
            .state
            .job(job_id)
            .ok_or_else(|| LedgerError::UnknownJob(job_id.into()))?;
        if js.job.paused || js.job.removed {
            return Err(LedgerError::JobInactive(job_id.into()));
        }
        if let Some(f) = self.state.claim_of(job_id, at) {
            return Err(LedgerError::AlreadyClaimed {
                job_id: job_id.into(),
                scheduled_for: at,
                fire_id: f.into(),
            });
        }
        if self.state.accounted(job_id).is_some_and(|a| a >= at) {
            return Err(LedgerError::AlreadyAccounted {
                job_id: job_id.into(),
                scheduled_for: at,
            });
        }
        Ok(())
    }

    /// Appends the terminal record of a run. A second settle of the same
    /// fire is refused: a `done` is never overwritten.
    pub fn settle(
        &mut self,
        fire_id: &str,
        status: RunStatus,
        session_id: Option<String>,
        usage: Option<Value>,
        now: i64,
    ) -> Result<Done, LedgerError> {
        self.settle_with(fire_id, status, session_id, None, usage, now)
    }

    fn settle_with(
        &mut self,
        fire_id: &str,
        status: RunStatus,
        session_id: Option<String>,
        reason: Option<String>,
        usage: Option<Value>,
        now: i64,
    ) -> Result<Done, LedgerError> {
        self.sync(true)?;
        let fire = self
            .state
            .fire(fire_id)
            .ok_or_else(|| LedgerError::UnknownFire(fire_id.into()))?;
        let done = Done {
            fire_id: fire_id.into(),
            job_id: fire.job_id.clone(),
            status,
            session_id,
            reason,
            usage,
            at: now,
        };
        if let Some(d) = self.state.done(fire_id) {
            return Err(LedgerError::AlreadySettled {
                fire_id: fire_id.into(),
                status: d.status,
            });
        }
        self.append(&Record::Done(done.clone()))?;
        match self.state.done(fire_id) {
            Some(d) if *d == done => Ok(done),
            Some(d) => Err(LedgerError::AlreadySettled {
                fire_id: fire_id.into(),
                status: d.status,
            }),
            None => Err(LedgerError::UnknownFire(fire_id.into())),
        }
    }

    /// Records a delivery attempt of a settled run. Never touches `done`.
    pub fn deliver(
        &mut self,
        fire_id: &str,
        target: &str,
        status: DeliveryStatus,
        error: Option<String>,
        now: i64,
    ) -> Result<Delivery, LedgerError> {
        self.sync(true)?;
        if self.state.done(fire_id).is_none() {
            return Err(match self.state.fire(fire_id) {
                Some(_) => LedgerError::NotSettled(fire_id.into()),
                None => LedgerError::UnknownFire(fire_id.into()),
            });
        }
        let attempt = self
            .state
            .deliveries(fire_id)
            .filter(|d| d.target == target)
            .count() as u32
            + 1;
        let d = Delivery {
            fire_id: fire_id.into(),
            target: target.into(),
            attempt,
            status,
            error,
            at: now,
        };
        self.append(&Record::Delivery(d.clone()))?;
        Ok(d)
    }

    /// Settles every pending fire whose lease is dead as `interrupted`.
    /// `alive` is the host's liveness check (pid exists AND its start time
    /// equals the lease's). No timeouts: a live owner is never touched.
    pub fn recover(
        &mut self,
        now: i64,
        alive: impl Fn(&Lease) -> bool,
    ) -> Result<Vec<Done>, LedgerError> {
        self.sync(true)?;
        let dead: Vec<String> = self
            .state
            .pending()
            .filter(|f| !alive(&f.lease))
            .map(|f| f.fire_id.clone())
            .collect();
        let mut out = Vec::new();
        for id in dead {
            let reason = Some("lease_dead".to_string());
            match self.settle_with(&id, RunStatus::Interrupted, None, reason, None, now) {
                Ok(d) => out.push(d),
                Err(LedgerError::AlreadySettled { .. }) => {} // settled meanwhile
                Err(e) => return Err(e),
            }
        }
        Ok(out)
    }

    /// Marks a webhook delivery as seen; `false` when it was seen before.
    /// Exclusivity relies on the host lock (unlike `claim`, which also
    /// re-checks after appending); the run it triggers still goes through
    /// `claim`.
    pub fn record_hook(
        &mut self,
        route: &str,
        delivery_id: &str,
        now: i64,
    ) -> Result<bool, LedgerError> {
        self.sync(true)?;
        if !valid_code(route, 128) || !valid_code(delivery_id, 256) {
            return Err(LedgerError::InvalidJob(
                "route and delivery id must be non-empty and bounded".into(),
            ));
        }
        if self.state.hook_seen(route, delivery_id) {
            return Ok(false);
        }
        let rec = HookSeen {
            route: route.into(),
            delivery_id: delivery_id.into(),
            at: now,
        };
        self.append(&Record::HookSeen(rec))?;
        Ok(true)
    }
}
