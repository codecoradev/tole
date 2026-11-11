//! File adapter for the automation ledger (#579): `<dir>/jobs.jsonl`
//! behind the core [`LedgerStore`] trait, plus the host lock and the
//! process lease the core ledger asks for (`docs/automation-design.md` §4).
//!
//! The core contract: an exclusive lock around every mutating ledger call,
//! whole-line appends, torn-tail repair only under that lock. The lock is
//! `<dir>/jobs.lock` (an advisory `flock`-style `File::lock`), held for
//! milliseconds — never around a run's execution.

use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use tole_core::ledger::{Lease, Ledger, LedgerError, LedgerStore};

/// The ledger file inside a cron dir.
pub fn ledger_path(dir: &Path) -> PathBuf {
    dir.join("jobs.jsonl")
}

/// `jobs.jsonl` on disk. Every append is one `write_all` of a whole line
/// on an `O_APPEND` handle followed by `sync_data`.
#[derive(Debug, Clone)]
pub struct FileStore {
    path: PathBuf,
}

impl FileStore {
    pub fn new(path: PathBuf) -> Self {
        Self { path }
    }
}

fn io(e: std::io::Error) -> LedgerError {
    LedgerError::Store(e.to_string())
}

impl LedgerStore for FileStore {
    fn read_from(&mut self, offset: u64) -> Result<Vec<u8>, LedgerError> {
        let mut f = match File::open(&self.path) {
            Ok(f) => f,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(io(e)),
        };
        f.seek(SeekFrom::Start(offset)).map_err(io)?;
        let mut buf = Vec::new();
        f.read_to_end(&mut buf).map_err(io)?;
        Ok(buf)
    }

    fn append(&mut self, line: &[u8]) -> Result<(), LedgerError> {
        let mut f = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
            .map_err(io)?;
        f.write_all(line).map_err(io)?;
        f.sync_data().map_err(io)
    }

    fn truncate(&mut self, len: u64) -> Result<(), LedgerError> {
        let f = OpenOptions::new()
            .write(true)
            .open(&self.path)
            .map_err(io)?;
        f.set_len(len).map_err(io)?;
        f.sync_data().map_err(io)
    }
}

/// Opens the ledger read-only (listings): no lock, never truncates.
pub fn open_read(dir: &Path) -> anyhow::Result<Ledger<FileStore>> {
    Ok(Ledger::open(FileStore::new(ledger_path(dir)))?)
}

/// Runs `f` on the ledger under the exclusive host lock.
pub fn with_lock<R>(
    dir: &Path,
    f: impl FnOnce(&mut Ledger<FileStore>) -> anyhow::Result<R>,
) -> anyhow::Result<R> {
    std::fs::create_dir_all(dir)?;
    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(dir.join("jobs.lock"))?;
    lock.lock()?;
    let mut ledger = Ledger::open(FileStore::new(ledger_path(dir)))?;
    let out = f(&mut ledger);
    drop(lock); // closing the handle releases the lock
    out
}

/// This process's lease: pid plus its start time (Linux `/proc` start
/// ticks; 0 where the platform does not expose it).
pub fn own_lease() -> Lease {
    let pid = std::process::id();
    Lease {
        pid,
        start_time: start_time(pid).unwrap_or(0),
    }
}

/// Process start time in clock ticks since boot (field 22 of
/// `/proc/<pid>/stat`), when available.
fn start_time(pid: u32) -> Option<u64> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    // The command name (field 2) may contain spaces: parse after the ')'.
    let rest = &stat[stat.rfind(')')? + 1..];
    rest.split_whitespace().nth(19)?.parse().ok()
}

/// Liveness of a lease: the pid exists AND (where start times are known)
/// it is the same process, not a reused pid. No timeouts.
pub fn lease_alive(lease: &Lease) -> bool {
    if lease.pid == std::process::id() {
        return start_time(lease.pid).unwrap_or(0) == lease.start_time;
    }
    if !pid_exists(lease.pid) {
        return false;
    }
    match start_time(lease.pid) {
        Some(t) => t == lease.start_time,
        None => true, // cannot tell: never interrupt a possibly live owner
    }
}

#[cfg(unix)]
fn pid_exists(pid: u32) -> bool {
    let Ok(pid) = libc::pid_t::try_from(pid) else {
        return false;
    };
    // SAFETY: signal 0 performs only the existence/permission check.
    let rc = unsafe { libc::kill(pid, 0) };
    rc == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

#[cfg(not(unix))]
fn pid_exists(_pid: u32) -> bool {
    true // conservative: an unknown owner is treated as alive
}
