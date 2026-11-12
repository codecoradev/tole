//! The machine-wide side of the delegation resource guard (owner
//! requirement: delegation must never overload or crash the server). The
//! decision itself is `tole_core::resource_guard`; this module gives it the
//! census and runs it before every delegated launch.
//!
//! Every delegated child started on this machine, by any session, goal or
//! depth, registers its `{pid, start_time}` lease in one small registry,
//! `<data dir>/delegation/machine.jsonl`, under its own lock
//! (`machine.lock`). That lock is only ever taken while a delegation log's
//! lock is held, never the other way round, so the two cannot deadlock; it
//! is held for the census, the probes and the spawn (milliseconds).
//!
//! The census: live children are entries whose lease is alive (an exited,
//! unreaped child counts as gone); children still ramping up are live ones
//! started less than [`RAMP_SECS`] ago (memory is reserved for each); the
//! last start is the newest entry. Dead entries older than that window are
//! dropped on the next admission (an atomic rewrite).
//!
//! The registry is outside every workspace. A child allowed to run
//! commands could still write it (the same authority it has over any path
//! outside the jail): a forged entry can only make spawns wait. It is
//! opened without following symlinks or blocking, and at most
//! [`REGISTRY_CAP`] bytes are read.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use tole_core::ledger::{Lease, LedgerStore as _};
use tole_core::resource_guard::{startup_notes, Admit, Census, LinuxProbe, Probe, Thresholds};

use crate::ledger_file::FileStore;

pub const REGISTRY: &str = "machine.jsonl";
pub const REGISTRY_CAP: u64 = 256 * 1024;
/// A child started less than this ago may not have reached its working
/// memory yet, so `child_memory_mb` stays reserved for it. One fixed window
/// is the simplest honest rule: a child's memory use cannot be predicted,
/// and its first provider call says nothing about its builds.
pub const RAMP_SECS: u64 = 30;
/// `tole doctor` warns about a spawn queued longer than this.
pub const STUCK_SECS: i64 = 600;

/// A probe shared by the hosts of one process (tests inject a fake one).
#[derive(Clone)]
pub struct SharedProbe(pub Arc<dyn Probe + Send + Sync>);

impl std::fmt::Debug for SharedProbe {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("SharedProbe")
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
struct Entry {
    pid: u32,
    start_time: u64,
    at: u64,
}

impl Entry {
    fn lease(&self) -> Lease {
        Lease {
            pid: self.pid,
            start_time: self.start_time,
        }
    }
}

/// The outcome of one admission.
pub enum Gate<T> {
    Started(T),
    Wait(String),
}

/// The machine guard of one host.
#[derive(Debug, Clone)]
pub struct Guard {
    pub thresholds: Thresholds,
    pub probe: SharedProbe,
    /// `<data dir>/delegation`: the registry and its lock.
    pub dir: PathBuf,
}

impl Guard {
    pub fn new(thresholds: Thresholds, probe: Option<SharedProbe>, dir: PathBuf) -> Self {
        Self {
            thresholds,
            probe: probe.unwrap_or_else(|| SharedProbe(Arc::new(LinuxProbe))),
            dir,
        }
    }

    fn registry(&self) -> PathBuf {
        self.dir.join(REGISTRY)
    }

    /// The census as the registry says now (no lock: a report).
    pub fn census(&self, now: u64, alive: &dyn Fn(&Lease) -> bool) -> Census {
        census_of(&read(&self.registry()), now, alive)
    }

    /// Under the registry lock: admits one spawn and runs `start`, which
    /// returns its output and the started child's lease (registered), or
    /// answers why it waits. `start` is never run on a wait.
    pub fn gate<T>(
        &self,
        paths: &[&Path],
        now: u64,
        alive: &dyn Fn(&Lease) -> bool,
        start: impl FnOnce() -> (T, Option<Lease>),
    ) -> Result<Gate<T>, String> {
        std::fs::create_dir_all(&self.dir).map_err(|e| format!("delegate: guard: {e}"))?;
        let lock = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(self.dir.join("machine.lock"))
            .map_err(|e| format!("delegate: guard lock: {e}"))?;
        lock.lock()
            .map_err(|e| format!("delegate: guard lock: {e}"))?;
        let file = self.registry();
        let all = read(&file);
        let window = RAMP_SECS.max(self.thresholds.spawn_interval_secs);
        let keep: Vec<Entry> = all
            .iter()
            .copied()
            .filter(|e| alive(&e.lease()) || now.saturating_sub(e.at) < window)
            .collect();
        if keep.len() != all.len() {
            rewrite(&file, &keep)?;
        }
        let census = census_of(&keep, now, alive);
        if let Admit::Wait { reason } = self.thresholds.admit(&*self.probe.0, census, paths) {
            return Ok(Gate::Wait(reason));
        }
        let (out, lease) = start();
        if let Some(l) = lease {
            let e = Entry {
                pid: l.pid,
                start_time: l.start_time,
                at: now,
            };
            let mut line = serde_json::to_vec(&e).map_err(|e| e.to_string())?;
            line.push(b'\n');
            // An unregistered child would be undercounted: report it.
            FileStore::new(file)
                .append(&line)
                .map_err(|e| format!("delegate: guard registry: {e}"))?;
        }
        drop(lock);
        Ok(Gate::Started(out))
    }
}

fn census_of(entries: &[Entry], now: u64, alive: &dyn Fn(&Lease) -> bool) -> Census {
    let live: Vec<&Entry> = entries.iter().filter(|e| alive(&e.lease())).collect();
    Census {
        live_machine: u32::try_from(live.len()).unwrap_or(u32::MAX),
        ramping: u32::try_from(
            live.iter()
                .filter(|e| now.saturating_sub(e.at) < RAMP_SECS)
                .count(),
        )
        .unwrap_or(u32::MAX),
        last_start: entries.iter().map(|e| e.at).max(),
        now,
    }
}

/// The registry's entries: no symlink followed, never blocking, at most
/// [`REGISTRY_CAP`] bytes; unreadable lines are skipped.
fn read(path: &Path) -> Vec<Entry> {
    let mut o = std::fs::OpenOptions::new();
    o.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        o.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    let Ok(f) = o.open(path) else {
        return Vec::new();
    };
    if !f.metadata().is_ok_and(|m| m.is_file()) {
        return Vec::new();
    }
    let mut buf = Vec::new();
    let _ = f.take(REGISTRY_CAP).read_to_end(&mut buf);
    String::from_utf8_lossy(&buf)
        .lines()
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect()
}

fn rewrite(path: &Path, entries: &[Entry]) -> Result<(), String> {
    let mut body = Vec::new();
    for e in entries {
        body.extend(serde_json::to_vec(e).map_err(|e| e.to_string())?);
        body.push(b'\n');
    }
    let tmp = path.with_extension("jsonl.tmp");
    let _ = std::fs::remove_file(&tmp);
    std::fs::write(&tmp, body)
        .and_then(|()| std::fs::rename(&tmp, path))
        .map_err(|e| format!("delegate: guard registry: {e}"))
}

/// `tole doctor`: the headroom against each threshold, the machine's live
/// delegated children, unreadable probes, and spawns queued longer than
/// [`STUCK_SECS`].
pub fn doctor_lines(setup: &crate::delegate::Setup, cwd: Option<&Path>, now: i64) -> Vec<String> {
    let Some(data) = setup.data_dir.as_deref() else {
        return vec!["  - delegation guard: no data dir".into()];
    };
    let g = setup.guard(data.join("delegation"));
    let t = &g.thresholds;
    let p = &*g.probe.0;
    let c = g.census(now.max(0) as u64, &crate::ledger_file::lease_alive);
    let mark = |ok: bool| if ok { "✓" } else { "!" };
    let mut out = vec!["delegation guard:".to_string()];
    if let Some(m) = p.memory() {
        let needed = t.min_free_memory_mb + t.child_memory_mb * (u64::from(c.ramping) + 1);
        out.push(format!(
            "  {} memory {} MB free (a spawn needs {needed} MB)",
            mark(m.available_mb >= needed),
            m.available_mb
        ));
    }
    if let Some(l) = p.load() {
        let per_cpu = l.one_min / f64::from(l.cpus.max(1));
        out.push(format!(
            "  {} load {per_cpu:.2} per CPU (max {:.2})",
            mark(per_cpu <= t.max_load_per_cpu),
            t.max_load_per_cpu
        ));
    }
    let mut paths = vec![data.to_path_buf()];
    paths.extend(cwd.map(Path::to_path_buf));
    for path in &paths {
        if let Some(d) = p.disk(path) {
            out.push(format!(
                "  {} disk {} MB free on {} (min {} MB)",
                mark(d.free_mb >= t.min_free_disk_mb),
                d.free_mb,
                path.display(),
                t.min_free_disk_mb
            ));
        }
    }
    if let Some(pi) = p.pids() {
        let free = pi.max.saturating_sub(pi.current);
        out.push(format!(
            "  {} process ids {} of {} in use (keeps {}% free)",
            mark(free.saturating_mul(100) >= t.min_free_pids_percent.saturating_mul(pi.max)),
            pi.current,
            pi.max,
            t.min_free_pids_percent
        ));
    }
    out.push(format!(
        "  {} {} of {} delegated children live on this machine",
        mark(c.live_machine < t.machine_max_live),
        c.live_machine,
        t.machine_max_live
    ));
    let refs: Vec<&Path> = paths.iter().map(PathBuf::as_path).collect();
    out.extend(
        startup_notes(p, &refs)
            .into_iter()
            .map(|n| format!("  ! {n}")),
    );
    out.extend(stuck(&data.join("delegation"), now));
    out
}

/// Spawns queued longer than [`STUCK_SECS`] in any delegation log.
fn stuck(dir: &Path, now: i64) -> Vec<String> {
    let mut out = Vec::new();
    for e in std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .take(256)
    {
        let log = e.path().join(crate::delegate::LOG_FILE);
        let Ok(d) = tole_core::delegation::Delegation::open(FileStore::new(log)) else {
            continue;
        };
        for n in d.queued() {
            let waited = now.saturating_sub(n.at);
            if waited > STUCK_SECS {
                out.push(format!(
                    "  ! delegated spawn `{}` has waited {} min in the queue: it starts on the \
                     next delegate or poll once the headroom above allows",
                    n.id,
                    waited / 60
                ));
            }
        }
    }
    out
}
