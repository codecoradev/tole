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
                .and_then(|p| match crate::config::user_config_present(&p) {
                    Ok(true) => crate::config::load(&p).ok(),
                    Ok(false) => None,
                    // Said once (this runs once per process); the default
                    // source applies, as for a file that does not parse.
                    Err(e) => {
                        eprintln!("tole: user config not applied for cron.tz_source: {e}");
                        None
                    }
                })
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

/// This process's lease: pid plus its start time (Linux: `/proc` start
/// ticks; macOS: microseconds since the epoch; 0 where the platform does
/// not expose it).
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

/// What the kernel says about `pid` on macOS, which has no `/proc`:
/// `proc_pidinfo` with the `PROC_PIDTBSDINFO` flavour (#1170). `Err` is the
/// `errno` of the refused call: `ESRCH` for a process that does not exist
/// and, checked on macOS 26.5.1, for a zombie too (while `kill(pid, 0)` still
/// succeeds for it); `EPERM` for a process the kernel keeps private.
#[cfg(target_os = "macos")]
#[allow(unsafe_code)] // SAFETY: pid probe FFI (kill 0, proc_pidinfo); see the blocks.
fn bsd_info(pid: u32) -> Result<libc::proc_bsdinfo, i32> {
    let pid = libc::c_int::try_from(pid).map_err(|_| libc::ESRCH)?;
    let size = libc::c_int::try_from(std::mem::size_of::<libc::proc_bsdinfo>())
        .map_err(|_| libc::EINVAL)?;
    // SAFETY: `proc_bsdinfo` is plain integers and byte arrays, so all-zero
    // is a valid value.
    let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
    // SAFETY: `info` is a live, writable buffer of exactly `size` bytes; the
    // call writes at most `size` bytes into it and returns how many it wrote.
    let wrote = unsafe {
        libc::proc_pidinfo(
            pid,
            libc::PROC_PIDTBSDINFO,
            0,
            std::ptr::addr_of_mut!(info).cast(),
            size,
        )
    };
    if wrote == size {
        Ok(info)
    } else {
        Err(std::io::Error::last_os_error()
            .raw_os_error()
            .unwrap_or(libc::EINVAL))
    }
}

/// A process that has exited but was not reaped yet (state `Z` in
/// `/proc/<pid>/stat`; on macOS `SZOMB`, or the kernel answering `ESRCH` for
/// a pid that `kill(pid, 0)` still finds) is not alive: it runs nothing and
/// never will. Called only for a pid that exists, so on macOS an `ESRCH` here
/// is a zombie, or a process that went away a moment ago: not alive either way.
fn zombie(pid: u32) -> bool {
    #[cfg(target_os = "macos")]
    {
        match bsd_info(pid) {
            Ok(info) => info.pbi_status == libc::SZOMB,
            Err(errno) => errno == libc::ESRCH,
        }
    }
    #[cfg(not(target_os = "macos"))]
    {
        std::fs::read_to_string(format!("/proc/{pid}/stat"))
            .ok()
            .and_then(|s| {
                let rest = &s[s.rfind(')')? + 1..];
                rest.split_whitespace().next().map(|st| st == "Z")
            })
            .unwrap_or(false)
    }
}

/// Process start time, when available: clock ticks since boot (field 22 of
/// `/proc/<pid>/stat`) on Linux, microseconds since the epoch on macOS.
fn start_time(pid: u32) -> Option<u64> {
    #[cfg(target_os = "macos")]
    {
        let info = bsd_info(pid).ok()?;
        info.pbi_start_tvsec
            .checked_mul(1_000_000)?
            .checked_add(info.pbi_start_tvusec)
    }
    #[cfg(not(target_os = "macos"))]
    {
        let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
        // The command name (field 2) may contain spaces: parse after the ')'.
        let rest = &stat[stat.rfind(')')? + 1..];
        rest.split_whitespace().nth(19)?.parse().ok()
    }
}

/// Is the process now running under a lease's pid the one that took the
/// lease? `recorded` is the start time stored in the lease, `now` the
/// current one of that pid.
///
/// A recorded 0 means "the start time was not known when the lease was
/// taken". Before #1170 macOS could not read one and stored 0 in every
/// lease, in ledgers, cron status files and the machine-guard registry that
/// may still be on disk. Such a lease cannot be compared, so on macOS it
/// counts as the same process (the rule below: never interrupt a possibly
/// live owner). Linux keeps comparing it as before.
fn same_start(recorded: u64, now: Option<u64>) -> bool {
    match now {
        Some(t) => t == recorded || (recorded == 0 && cfg!(target_os = "macos")),
        None => true, // cannot tell: never interrupt a possibly live owner
    }
}

/// Liveness of a lease: the pid exists AND (where start times are known)
/// it is the same process, not a reused pid. No timeouts.
pub fn lease_alive(lease: &Lease) -> bool {
    if lease.pid == std::process::id() {
        let now = start_time(lease.pid);
        return match now {
            Some(_) => same_start(lease.start_time, now),
            None => lease.start_time == 0,
        };
    }
    if !pid_exists(lease.pid) || zombie(lease.pid) {
        return false;
    }
    same_start(lease.start_time, start_time(lease.pid))
}

#[cfg(unix)]
#[allow(unsafe_code)] // SAFETY: pid probe FFI (kill 0, proc_pidinfo); see the blocks.
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

#[cfg(all(test, any(target_os = "linux", target_os = "macos")))]
mod tests {
    use super::*;

    /// An exited child its parent never reaped (a detached delegated or
    /// agent child) is a zombie: not alive, so recovery can settle it.
    #[test]
    fn an_unreaped_exited_child_is_not_alive() {
        let child = std::process::Command::new("true").spawn().unwrap();
        let lease = lease_of(child.id());
        crate::test_dir::wait_until(
            crate::test_dir::WAIT,
            "the exited child to stop counting as alive",
            || !lease_alive(&lease),
        );
        assert!(lease_alive(&own_lease()), "this process is alive");
        drop(child); // never waited: stays a zombie until the test ends
    }

    /// A live process under a lease with its real start time is alive; the
    /// same pid with another start time is a reused pid, not the owner; and
    /// once the process is gone and reaped the lease is not alive.
    #[test]
    fn a_live_child_is_alive_only_with_its_own_start_time() {
        let mut child = std::process::Command::new("sleep")
            .arg("60")
            .spawn()
            .unwrap();
        let lease = lease_of(child.id());
        assert_ne!(
            lease.start_time, 0,
            "the start time of a live child is known"
        );
        assert!(lease_alive(&lease), "the live child");
        let reused = Lease {
            start_time: lease.start_time + 1,
            ..lease
        };
        assert!(!lease_alive(&reused), "the same pid, another start time");
        child.kill().unwrap();
        child.wait().unwrap();
        assert!(!lease_alive(&lease), "the reaped child");
    }

    /// The start time of this process is readable, and stable.
    #[test]
    fn this_process_has_a_stable_start_time() {
        let pid = std::process::id();
        let first = start_time(pid).expect("the start time of this process");
        assert_ne!(first, 0);
        assert_eq!(start_time(pid), Some(first));
        assert_eq!(own_lease().start_time, first);
    }

    /// macOS wrote 0 in every lease before #1170 (it could not read a start
    /// time), and those leases may still be in a ledger, a cron status file
    /// or the machine-guard registry. A live owner must not be declared a
    /// reused pid because of that 0.
    #[cfg(target_os = "macos")]
    #[test]
    fn a_lease_recorded_without_a_start_time_still_counts_a_live_owner() {
        let mut child = std::process::Command::new("sleep")
            .arg("60")
            .spawn()
            .unwrap();
        let legacy = Lease {
            pid: child.id(),
            start_time: 0,
        };
        assert!(lease_alive(&legacy), "a live child with a legacy lease");
        let own = Lease {
            pid: std::process::id(),
            start_time: 0,
        };
        assert!(lease_alive(&own), "this process with a legacy lease");
        child.kill().unwrap();
        child.wait().unwrap();
        assert!(!lease_alive(&legacy), "the legacy lease of a dead child");
    }

    /// On Linux a recorded 0 is still compared, as before #1170.
    #[cfg(target_os = "linux")]
    #[test]
    fn on_linux_a_recorded_zero_is_still_compared() {
        assert!(!same_start(0, Some(5)));
        assert!(same_start(5, Some(5)));
        assert!(same_start(0, None));
    }
}
