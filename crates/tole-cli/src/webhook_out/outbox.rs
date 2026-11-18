//! The outbox file format and its fold (#911, design section 4). Pure: bytes
//! in, state out. Nothing here opens a file, takes a lock or reads a clock;
//! the sender (a later part) does that and calls into this.
//!
//! `<cron dir>/outbox.jsonl` is append-only JSONL. One `enqueue` line holds the
//! redacted body once, so every attempt is byte-identical; `lease`, `attempt`,
//! `done` and `dead` lines are keyed by (destination, id), never by id alone.
//!
//! **The file is untrusted input.** 0600 does not stop another process of the
//! same account, and the sender signs whatever a line says. So the fold is
//! defensive (a bounded line, a torn tail ignored, a line with no `enqueue`
//! ignored, a far-future lease ignored) and [`validate`] bounds what a line may
//! make the sender sign. Forged `done`/`dead` lines or a far-future lease are an
//! availability attack by the same account, the accepted residual of the
//! same-account threat model; these rules are cheap hardening, not a defence.

use std::collections::{btree_map, BTreeMap};

use serde::{Deserialize, Serialize};

use super::{event::MAX_BODY, Destination, Event};

/// A line, without its newline, at most. A longer one is skipped and counted.
pub const MAX_LINE: usize = 32 * 1024;
/// How long a cron event may be retried (design section 4).
pub const CRON_DEADLINE_SECS: i64 = 6 * 3600;
/// Clock skew allowed on a body's `at` and a lease.
pub const SKEW_SECS: i64 = 60;
/// Pending events per destination the file may hold; past it a producer
/// refuses (`queue_full`).
pub const MAX_PENDING_PER_DESTINATION: usize = 10_000;
/// The file is never rewritten, so it is capped: past this a producer refuses.
pub const MAX_FILE_BYTES: u64 = 64 * 1024 * 1024;
/// `tole doctor` warns from here.
pub const WARN_FILE_BYTES: u64 = 32 * 1024 * 1024;

/// The closed set of codes this part can give a line that is never sent.
pub const BAD_LINE: &str = "bad_line";
pub const EXPIRED: &str = "expired";

/// One outbox line.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "k", rename_all = "snake_case")]
pub enum Line {
    /// An event to send. `body` is the exact text that is signed and sent.
    Enqueue {
        dest: String,
        id: String,
        at: i64,
        deadline: i64,
        body: String,
    },
    /// A drainer took the event until `lease_until`.
    Lease {
        dest: String,
        id: String,
        at: i64,
        lease_until: i64,
    },
    /// One result. `n` counts attempts and is only ever given by these lines.
    Attempt {
        dest: String,
        id: String,
        n: u32,
        at: i64,
        outcome: String,
        next_at: i64,
    },
    /// Delivered.
    Done { dest: String, id: String, at: i64 },
    /// Given up, with a code.
    Dead {
        dest: String,
        id: String,
        at: i64,
        code: String,
    },
}

impl Line {
    /// An `enqueue` for `body`, retried until [`CRON_DEADLINE_SECS`] after `now`.
    pub fn enqueue(dest: &str, id: &str, body: &str, now: i64) -> Line {
        Line::Enqueue {
            dest: dest.into(),
            id: id.into(),
            at: now,
            deadline: now.saturating_add(CRON_DEADLINE_SECS),
            body: body.into(),
        }
    }

    fn key(&self) -> (&str, &str) {
        match self {
            Line::Enqueue { dest, id, .. }
            | Line::Lease { dest, id, .. }
            | Line::Attempt { dest, id, .. }
            | Line::Done { dest, id, .. }
            | Line::Dead { dest, id, .. } => (dest, id),
        }
    }
}

/// Why a line could not be encoded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EncodeError {
    /// Over [`MAX_LINE`] once serialized.
    TooLong,
    Serialize,
}

/// The bytes to append for `line`: one JSON object and a newline.
pub fn encode(line: &Line) -> Result<Vec<u8>, EncodeError> {
    let mut bytes = serde_json::to_vec(line).map_err(|_| EncodeError::Serialize)?;
    if bytes.len() > MAX_LINE {
        return Err(EncodeError::TooLong);
    }
    bytes.push(b'\n');
    Ok(bytes)
}

/// What a writer puts before its line: a newline when the file does not end in
/// one. With several append-only writers, a crash mid-append leaves a last line
/// without `\n`, and the next line would be glued onto it. The fold counts the
/// fragment as one bad line and goes on.
pub fn separator(last_byte: Option<u8>) -> &'static [u8] {
    match last_byte {
        Some(b) if b != b'\n' => b"\n",
        _ => b"",
    }
}

/// How an event stands.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Terminal {
    Done,
    Dead(String),
}

/// What the fold keeps of one event. The body stays on disk: `offset` is where
/// its `enqueue` line starts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub offset: usize,
    pub at: i64,
    pub deadline: i64,
    /// Attempt results seen.
    pub attempts: u32,
    /// Not before this (0 until a result says so).
    pub next_at: i64,
    /// Leased until this (0 when not leased).
    pub lease_until: i64,
    pub terminal: Option<Terminal>,
}

impl Entry {
    pub fn pending(&self) -> bool {
        self.terminal.is_none()
    }
}

/// How the fold judges time-bound lines.
#[derive(Debug, Clone, Copy)]
pub struct FoldOptions {
    pub now: i64,
    /// The longest lease a line may claim past `now`: the largest destination
    /// timeout + 5 s, plus [`SKEW_SECS`]. A line claiming more is ignored, so a
    /// forged far-future lease cannot park an event.
    pub max_lease_ahead: i64,
}

/// The outbox as folded so far. Feed it the file (or the tail since the last
/// call) with [`Fold::feed`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Fold {
    entries: BTreeMap<(String, String), Entry>,
    /// Lines skipped for being too long, unparseable or a torn fragment that a
    /// later writer terminated.
    pub bad_lines: usize,
    /// Well-formed lines that were ignored (no `enqueue`, a repeat, a forged
    /// lease, a line after a terminal one).
    pub ignored: usize,
}

impl Fold {
    /// Folds the complete lines of `chunk`, which starts at file offset `base`,
    /// and returns how many bytes it consumed. A last line without a newline is
    /// a torn tail: it is not consumed, so the next call (from `base +
    /// consumed`) sees it whole once it is completed.
    pub fn feed(&mut self, chunk: &[u8], base: usize, opts: FoldOptions) -> usize {
        let mut pos = 0;
        while let Some(rel) = chunk[pos..].iter().position(|&b| b == b'\n') {
            let line = &chunk[pos..pos + rel];
            self.apply(line, base + pos, opts);
            pos += rel + 1;
        }
        pos
    }

    fn apply(&mut self, raw: &[u8], offset: usize, opts: FoldOptions) {
        if raw.is_empty() {
            return;
        }
        if raw.len() > MAX_LINE {
            self.bad_lines += 1;
            return;
        }
        let Ok(line) = serde_json::from_slice::<Line>(raw) else {
            self.bad_lines += 1;
            return;
        };
        let (dest, id) = (line.key().0.to_string(), line.key().1.to_string());
        let key = (dest, id);
        match line {
            Line::Enqueue {
                at,
                deadline,
                dest: _,
                id: _,
                body: _,
            } => {
                match self.entries.entry(key) {
                    btree_map::Entry::Occupied(_) => self.ignored += 1, // the first wins
                    btree_map::Entry::Vacant(v) => {
                        v.insert(Entry {
                            offset,
                            at,
                            deadline,
                            attempts: 0,
                            next_at: 0,
                            lease_until: 0,
                            terminal: None,
                        });
                    }
                }
            }
            Line::Lease { lease_until, .. } => match self.live(&key) {
                Some(e) if lease_until <= opts.now.saturating_add(opts.max_lease_ahead) => {
                    e.lease_until = lease_until
                }
                _ => self.ignored += 1,
            },
            Line::Attempt { n, next_at, .. } => match self.live(&key) {
                Some(e) => {
                    e.attempts = e.attempts.max(n);
                    e.next_at = next_at;
                    e.lease_until = 0; // a result ends the lease
                }
                None => self.ignored += 1,
            },
            Line::Done { .. } => match self.live(&key) {
                Some(e) => e.terminal = Some(Terminal::Done),
                None => self.ignored += 1,
            },
            Line::Dead { code, .. } => match self.live(&key) {
                Some(e) => e.terminal = Some(Terminal::Dead(code)),
                None => self.ignored += 1,
            },
        }
    }

    /// An entry that exists and has no terminal line yet.
    fn live(&mut self, key: &(String, String)) -> Option<&mut Entry> {
        self.entries.get_mut(key).filter(|e| e.pending())
    }

    pub fn get(&self, dest: &str, id: &str) -> Option<&Entry> {
        self.entries.get(&(dest.to_string(), id.to_string()))
    }

    /// Whether an enqueue for (destination, id) is already in the file, in any
    /// state: the producer's check-then-append and the reconcile pass use it.
    pub fn has(&self, dest: &str, id: &str) -> bool {
        self.get(dest, id).is_some()
    }

    /// Events of `dest` with no terminal line.
    pub fn pending_count(&self, dest: &str) -> usize {
        self.entries
            .iter()
            .filter(|((d, _), e)| d == dest && e.pending())
            .count()
    }

    /// Whether a producer must refuse one more event for `dest`.
    pub fn queue_full(&self, dest: &str) -> bool {
        self.pending_count(dest) >= MAX_PENDING_PER_DESTINATION
    }

    /// The one event per destination a drainer may lease at `now`: pending,
    /// before its deadline, not leased, not waiting on a retry time. The oldest
    /// first, so a destination is served in the order its events were enqueued.
    /// Returns `(destination, id, offset)`.
    pub fn due(&self, now: i64) -> Vec<(&str, &str, usize)> {
        let mut best: BTreeMap<&str, (&str, &Entry)> = BTreeMap::new();
        for ((d, id), e) in &self.entries {
            if !e.pending() || e.deadline <= now || e.next_at > now || e.lease_until > now {
                continue;
            }
            let better = best
                .get(d.as_str())
                .is_none_or(|(_, cur)| (e.next_at, e.offset) < (cur.next_at, cur.offset));
            if better {
                best.insert(d, (id, e));
            }
        }
        best.into_iter()
            .map(|(d, (id, e))| (d, id, e.offset))
            .collect()
    }

    /// Pending events whose deadline has passed: the sender marks each
    /// `dead:expired` and never sends it.
    pub fn expired(&self, now: i64) -> Vec<(&str, &str)> {
        self.entries
            .iter()
            .filter(|(_, e)| e.pending() && e.deadline <= now)
            .map(|((d, id), _)| (d.as_str(), id.as_str()))
            .collect()
    }

    /// Every entry, for a doctor view.
    pub fn entries(&self) -> impl Iterator<Item = (&str, &str, &Entry)> {
        self.entries
            .iter()
            .map(|((d, id), e)| (d.as_str(), id.as_str(), e))
    }
}

/// Re-checks an `enqueue` line before every send (the sender treats the file as
/// untrusted). `Err` is the code the event is marked `dead` with; it is never
/// sent. `destination` is the loaded destination named by the line, if any.
///
/// The body must be at most [`MAX_BODY`]; parse as schema v1; name this
/// destination, which must be configured; have a `type` that destination lists
/// in `events`; carry the line's own id, which must be a valid delivery id; and
/// have an `at` that is neither in the future nor past the deadline.
pub fn validate(
    dest: &str,
    id: &str,
    body: &str,
    destination: Option<&Destination>,
    now: i64,
) -> Result<(), &'static str> {
    let Some(destination) = destination else {
        return Err(BAD_LINE);
    };
    if body.len() > MAX_BODY || !tole_core::webhook::valid_delivery(id) {
        return Err(BAD_LINE);
    }
    let Ok(v) = serde_json::from_str::<serde_json::Value>(body) else {
        return Err(BAD_LINE);
    };
    let text = |k: &str| v.get(k).and_then(|x| x.as_str());
    if v.get("v").and_then(|x| x.as_i64()) != Some(1)
        || text("destination") != Some(dest)
        || text("id") != Some(id)
    {
        return Err(BAD_LINE);
    }
    match text("type").and_then(Event::parse) {
        Some(e) if destination.events.contains(&e) => {}
        _ => return Err(BAD_LINE),
    }
    let Some(at) = v.get("at").and_then(|x| x.as_i64()) else {
        return Err(BAD_LINE);
    };
    // `at` is read from the file: saturating, so a crafted i64::MIN or
    // i64::MAX cannot overflow (a panic in debug, a wrap in release).
    if at > now.saturating_add(SKEW_SECS) {
        return Err(BAD_LINE);
    }
    if now.saturating_sub(at) > CRON_DEADLINE_SECS {
        return Err(EXPIRED);
    }
    Ok(())
}

#[cfg(test)]
#[path = "outbox_tests.rs"]
mod tests;
