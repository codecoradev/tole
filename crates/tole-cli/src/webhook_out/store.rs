//! The outbox file (#911, design section 4): the only place that opens it.
//!
//! `<cron dir>/outbox.jsonl` is append-only; `<cron dir>/outbox.lock` is an
//! advisory file lock (released by the OS when the process dies, so there is
//! no stale lock). Every append takes the lock **blocking** for the short
//! append only. A drain pass takes it with `try_lock` for the leasing step
//! only. Network I/O never happens under it, and it is never held while
//! the ledger lock is (the two are never nested).
//!
//! Appends are one `write` of a whole line, preceded by a newline when the
//! file does not end in one, so a crash mid-append cannot glue the next
//! writer's line onto a fragment. The file is never rewritten, renamed or
//! truncated.

use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use super::outbox::{
    self, EncodeError, Fold, FoldOptions, Line, MAX_FILE_BYTES, MAX_LINE, SKEW_SECS,
};

/// A producer's failure, as the ledger `error` code it records.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EnqueueError {
    /// 10 000 pending events for this destination, or the file is at its cap.
    QueueFull,
    /// The line could not be encoded or written (disk full, I/O error).
    Failed,
}

impl EnqueueError {
    /// The closed code a ledger `delivery` row carries.
    pub fn code(self) -> &'static str {
        match self {
            EnqueueError::QueueFull => "queue_full",
            EnqueueError::Failed => "enqueue_failed",
        }
    }
}

/// What an enqueue did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Enqueued {
    Queued,
    /// (destination, id) is already in the file, in any state.
    Duplicate,
}

/// The outbox of one cron dir.
#[derive(Debug, Clone)]
pub struct Outbox {
    file: PathBuf,
    lock: PathBuf,
}

/// An event leased to this process, with the body to send.
#[derive(Clone, PartialEq, Eq)]
pub struct Leased {
    pub dest: String,
    pub id: String,
    /// The run this event reports (`data.fire_id`), for the ledger row.
    pub fire_id: Option<String>,
    pub body: String,
    /// The lease runs out at this time; check it again just before sending.
    pub lease_until: i64,
    /// Attempt results already recorded; the next attempt is `attempts + 1`.
    pub attempts: u32,
    pub deadline: i64,
}

/// What a leasing step found.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Leasing {
    /// One event per destination, each leased until its destination's timeout
    /// + 5 s.
    pub leased: Vec<Leased>,
    /// Events now marked `dead` (past their deadline, or a body that does not
    /// read back); the caller writes their ledger rows.
    pub dead: Vec<Dead>,
    /// False when another drainer holds the lock: nothing was done.
    pub got_lock: bool,
}

/// An event this step marked `dead`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Dead {
    pub dest: String,
    pub id: String,
    pub fire_id: Option<String>,
    pub code: &'static str,
}

// The body is redacted but private run output: `Debug` shows the shape only.
impl std::fmt::Debug for Leased {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Leased")
            .field("dest", &self.dest)
            .field("id", &self.id)
            .field("attempts", &self.attempts)
            .finish_non_exhaustive()
    }
}

impl Outbox {
    pub fn new(cron_dir: &Path) -> Outbox {
        Outbox {
            file: cron_dir.join("outbox.jsonl"),
            lock: cron_dir.join("outbox.lock"),
        }
    }

    /// The file's size, 0 when it does not exist yet.
    pub fn size(&self) -> u64 {
        std::fs::metadata(&self.file).map_or(0, |m| m.len())
    }

    fn open_lock(&self) -> std::io::Result<File> {
        if let Some(dir) = self.lock.parent() {
            std::fs::create_dir_all(dir)?;
        }
        OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&self.lock)
    }

    fn open_append(&self) -> std::io::Result<File> {
        let mut o = OpenOptions::new();
        o.read(true).append(true).create(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            // 0600: the file holds redacted but private run output. NOFOLLOW:
            // a planted symlink must not redirect the append.
            o.mode(0o600).custom_flags(libc::O_NOFOLLOW);
        }
        o.open(&self.file)
    }

    /// Reads the whole file (a missing one is empty). Reading needs no lock:
    /// the file is append-only and the fold ignores a torn tail. The file is
    /// untrusted: it is opened without following a symlink and without
    /// blocking (a planted FIFO), must be a regular file, and a file over the
    /// cap is refused instead of read, so a planted huge file cannot exhaust
    /// memory.
    pub fn read_all(&self) -> std::io::Result<Vec<u8>> {
        let mut o = OpenOptions::new();
        o.read(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            o.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
        }
        let f = match o.open(&self.file) {
            Ok(f) => f,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(e),
        };
        let meta = f.metadata()?;
        if !meta.is_file() {
            return Err(std::io::Error::other("the outbox is not a regular file"));
        }
        if meta.len() > MAX_FILE_BYTES.saturating_mul(2) {
            return Err(std::io::Error::other("the outbox is far over its size cap"));
        }
        let mut v = Vec::new();
        f.take(MAX_FILE_BYTES.saturating_mul(2) + 1)
            .read_to_end(&mut v)?;
        Ok(v)
    }

    /// The fold of the whole file.
    pub fn fold(&self, opts: FoldOptions) -> std::io::Result<Fold> {
        let bytes = self.read_all()?;
        let mut fold = Fold::default();
        fold.feed(&bytes, 0, opts);
        Ok(fold)
    }

    /// Appends `lines` in one write each, under the blocking lock.
    pub fn append(&self, lines: &[Line]) -> Result<(), EnqueueError> {
        let lock = self.open_lock().map_err(|_| EnqueueError::Failed)?;
        lock.lock().map_err(|_| EnqueueError::Failed)?;
        let r = self.append_locked(lines);
        drop(lock);
        r
    }

    /// Appends with the lock already held by the caller.
    fn append_locked(&self, lines: &[Line]) -> Result<(), EnqueueError> {
        let mut f = self.open_append().map_err(|_| EnqueueError::Failed)?;
        let last = last_byte(&mut f).map_err(|_| EnqueueError::Failed)?;
        let mut buf = outbox::separator(last).to_vec();
        for l in lines {
            buf.extend(outbox::encode(l).map_err(|e| match e {
                EncodeError::TooLong | EncodeError::Serialize => EnqueueError::Failed,
            })?);
        }
        f.write_all(&buf).map_err(|_| EnqueueError::Failed)
    }

    /// The producer's check-then-append, atomic under the lock: refuses when
    /// the file or the destination's queue is full, and skips an event that is
    /// already there. `now` stamps the line; the body is stored once.
    pub fn enqueue(
        &self,
        dest: &str,
        id: &str,
        body: &str,
        now: i64,
    ) -> Result<Enqueued, EnqueueError> {
        let lock = self.open_lock().map_err(|_| EnqueueError::Failed)?;
        lock.lock().map_err(|_| EnqueueError::Failed)?;
        let r = (|| {
            if self.size() >= MAX_FILE_BYTES {
                return Err(EnqueueError::QueueFull);
            }
            let fold = self
                .fold(FoldOptions {
                    now,
                    max_lease_ahead: 0,
                })
                .map_err(|_| EnqueueError::Failed)?;
            if fold.has(dest, id) {
                return Ok(Enqueued::Duplicate);
            }
            if fold.queue_full(dest) {
                return Err(EnqueueError::QueueFull);
            }
            self.append_locked(&[Line::enqueue(dest, id, body, now)])?;
            Ok(Enqueued::Queued)
        })();
        drop(lock);
        r
    }

    /// The leasing step of a drain pass. With `try_lock`: a pass that does not
    /// get the lock does nothing. Under it: folds, marks events past their
    /// deadline `dead:expired`, and leases one due event per destination.
    /// `lease_for` gives a destination's lease length (its timeout + 5 s);
    /// `None` means the destination is not configured, so it is left alone
    /// (the sender never signs for a destination the config does not hold).
    pub fn lease_due(
        &self,
        now: i64,
        lease_for: &dyn Fn(&str) -> Option<i64>,
        max_timeout: i64,
    ) -> std::io::Result<Leasing> {
        let lock = self.open_lock()?;
        match lock.try_lock() {
            Ok(()) => {}
            Err(std::fs::TryLockError::WouldBlock) => return Ok(Leasing::default()),
            Err(std::fs::TryLockError::Error(e)) => return Err(e),
        }
        let out = self.lease_locked(now, lease_for, max_timeout);
        drop(lock);
        out
    }

    fn lease_locked(
        &self,
        now: i64,
        lease_for: &dyn Fn(&str) -> Option<i64>,
        max_timeout: i64,
    ) -> std::io::Result<Leasing> {
        let bytes = self.read_all()?;
        let mut fold = Fold::default();
        fold.feed(
            &bytes,
            0,
            FoldOptions {
                now,
                max_lease_ahead: max_timeout + 5 + SKEW_SECS,
            },
        );
        let mut out = Leasing {
            got_lock: true,
            ..Leasing::default()
        };
        let mut lines = Vec::new();
        let kill = |lines: &mut Vec<Line>,
                    out: &mut Leasing,
                    d: &str,
                    id: &str,
                    fire_id: Option<String>,
                    code: &'static str| {
            lines.push(Line::Dead {
                dest: d.into(),
                id: id.into(),
                at: now,
                code: code.into(),
            });
            out.dead.push(Dead {
                dest: d.into(),
                id: id.into(),
                fire_id,
                code,
            });
        };
        for (d, id) in fold.expired(now) {
            let fire = fold
                .get(d, id)
                .and_then(|e| body_at(&bytes, e.offset))
                .and_then(|b| fire_id_of(&b, id));
            kill(&mut lines, &mut out, d, id, fire, outbox::EXPIRED);
        }
        for (d, id, offset) in fold.due(now) {
            let Some(secs) = lease_for(d) else { continue };
            let Some(body) = body_at(&bytes, offset) else {
                // The fold saw an `enqueue` here; a body that no longer reads
                // back is a bad line, never sent.
                kill(&mut lines, &mut out, d, id, None, outbox::BAD_LINE);
                continue;
            };
            let entry = fold.get(d, id);
            let lease_until = now.saturating_add(secs);
            lines.push(Line::Lease {
                dest: d.into(),
                id: id.into(),
                at: now,
                lease_until,
            });
            out.leased.push(Leased {
                dest: d.into(),
                id: id.into(),
                fire_id: fire_id_of(&body, id),
                body,
                lease_until,
                attempts: entry.map_or(0, |e| e.attempts),
                deadline: entry.map_or(0, |e| e.deadline),
            });
        }
        if !lines.is_empty() {
            self.append_locked(&lines)
                .map_err(|_| std::io::Error::other("outbox append failed"))?;
        }
        Ok(out)
    }
}

/// The last byte of the file, if it has any.
fn last_byte(f: &mut File) -> std::io::Result<Option<u8>> {
    let len = f.metadata()?.len();
    if len == 0 {
        return Ok(None);
    }
    f.seek(SeekFrom::Start(len - 1))?;
    let mut b = [0u8; 1];
    f.read_exact(&mut b)?;
    Ok(Some(b[0]))
}

/// `data.fire_id` of a body, only when `id` is the event id of exactly that
/// run (`cron_event_id` is a pure function of the fire id). The file is
/// untrusted: without this check a forged line could name another run's fire
/// id and make the host write a row against it. A fire id that event.rs cut
/// at 256 characters does not match either, so it gives `None` (no row)
/// rather than a row on a run that does not exist.
fn fire_id_of(body: &str, id: &str) -> Option<String> {
    let fire = serde_json::from_str::<serde_json::Value>(body)
        .ok()?
        .get("data")?
        .get("fire_id")?
        .as_str()?
        .to_string();
    (super::event::cron_event_id(&fire) == id).then_some(fire)
}

/// The `body` of the `enqueue` line that starts at `offset` in `bytes`, read
/// again from the file (the fold keeps no body).
fn body_at(bytes: &[u8], offset: usize) -> Option<String> {
    let rest = bytes.get(offset..)?;
    let end = rest.iter().position(|&b| b == b'\n')?;
    let line = rest.get(..end).filter(|l| l.len() <= MAX_LINE)?;
    match serde_json::from_slice::<Line>(line).ok()? {
        Line::Enqueue { body, .. } => Some(body),
        _ => None,
    }
}

#[cfg(test)]
#[path = "store_tests.rs"]
mod tests;
