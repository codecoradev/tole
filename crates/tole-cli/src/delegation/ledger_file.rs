//! File adapter for the append-only ledgers behind the core
//! [`LedgerStore`] trait — the cron job store `<dir>/jobs.jsonl` (#579) and
//! the delegation log (#585) — plus the host lock and the process lease the
//! core asks for (`docs/automation-design.md` §4). Reached as
//! `crate::cron::store` too.
//!
//! The core contract: an exclusive lock around every mutating ledger call,
//! whole-line appends, torn-tail repair only under that lock. For cron the
//! lock is `<dir>/jobs.lock` (an advisory `flock`-style `File::lock`), held
//! for milliseconds — never around a run's execution.

use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};

use tole_core::ledger::{Lease, Ledger, LedgerError, LedgerStore};
use tole_core::tz::{OffsetOnly, ZoneSource, ZoneinfoDir};

use crate::config::TzSource;

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

/// Where job time zones resolve (#580): the user config's
/// `cron.tz_source`, by default `system` on unix (`$TZDIR`, else
/// `/usr/share/zoneinfo`, read at run time) and `offset-only` elsewhere.
/// Read once per process.
pub fn zones() -> Arc<dyn ZoneSource> {
    static ZONES: OnceLock<Arc<dyn ZoneSource>> = OnceLock::new();
    ZONES
        .get_or_init(|| {
            let configured = crate::config::user_config_path()
                .filter(|p| p.exists())
                .and_then(|p| crate::config::load(&p).ok())
                .and_then(|c| c.cron?.tz_source);
            let system = || -> Arc<dyn ZoneSource> {
                match std::env::var_os("TZDIR").filter(|d| !d.is_empty()) {
                    Some(d) => Arc::new(ZoneinfoDir::new(PathBuf::from(d))),
                    None => Arc::new(ZoneinfoDir::system()),
                }
            };
            match configured {
                Some(TzSource::System) => system(),
                Some(TzSource::OffsetOnly) => Arc::new(OffsetOnly),
                None if cfg!(unix) => system(),
                None => Arc::new(OffsetOnly),
            }
        })
        .clone()
}

/// Opens the ledger read-only (listings): no lock, never truncates.
pub fn open_read(dir: &Path) -> anyhow::Result<Ledger<FileStore>> {
    Ok(Ledger::open_with_zones(
        FileStore::new(ledger_path(dir)),
        zones(),
    )?)
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
    let mut ledger = Ledger::open_with_zones(FileStore::new(ledger_path(dir)), zones())?;
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

/// The lease of process `pid` (pid plus its start time where known).
pub fn lease_of(pid: u32) -> Lease {
    Lease {
        pid,
        start_time: start_time(pid).unwrap_or(0),
    }
}

/// A process that has exited but was not reaped yet (state `Z` in
/// `/proc/<pid>/stat`) is not alive: it runs nothing and never will.
fn zombie(pid: u32) -> bool {
    std::fs::read_to_string(format!("/proc/{pid}/stat"))
        .ok()
        .and_then(|s| {
            let rest = &s[s.rfind(')')? + 1..];
            rest.split_whitespace().next().map(|st| st == "Z")
        })
        .unwrap_or(false)
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
    if !pid_exists(lease.pid) || zombie(lease.pid) {
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

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;

    /// An exited child its parent never reaped (a detached delegated or
    /// agent child) is a zombie: not alive, so recovery can settle it.
    #[test]
    fn an_unreaped_exited_child_is_not_alive() {
        let child = std::process::Command::new("true").spawn().unwrap();
        let lease = lease_of(child.id());
        let mut gone = false;
        for _ in 0..200 {
            if !lease_alive(&lease) {
                gone = true;
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert!(gone, "a zombie counted as alive");
        assert!(lease_alive(&own_lease()), "this process is alive");
        drop(child); // never waited: stays a zombie until the test ends
    }
}
