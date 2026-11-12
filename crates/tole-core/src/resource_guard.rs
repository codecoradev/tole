//! Machine resource guard for delegation (#585 follow-up, owner
//! requirement): a delegated child is started only when the machine has
//! room for it, so delegation from the orchestrator (depth 0) or the agent
//! below it (depth 1) can never overload or crash the server.
//!
//! - [`Probe`] reads memory, load, disk and pids. [`LinuxProbe`] reads them
//!   from cgroup v2 and `/proc` (pure parse functions below, tested on
//!   fixtures); tests use fixed probes, so every decision is deterministic.
//! - [`Thresholds`] are the limits (user config `[delegation]` keys).
//! - [`Thresholds::admit`] answers [`Admit::Go`] or [`Admit::Wait`] with a
//!   reason. A `Wait` is a queue decision, never an error: the spawn waits
//!   in the queue and starts when the machine recovers.
//! - A probe that cannot be read (another OS, no permission) skips its
//!   check, reported once by [`startup_notes`]: it is neither "plenty" nor
//!   a reason to refuse forever.
//! - [`apply_child_nice`] lowers a delegated child's CPU priority so the
//!   orchestrator and the server stay responsive.

use std::path::Path;

/// Memory headroom: what a new process can still use, in MiB.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Mem {
    pub available_mb: u64,
}

/// One-minute load average and the CPUs it is shared by.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Load {
    pub one_min: f64,
    pub cpus: u32,
}

/// Free space for an unprivileged user on one filesystem, in MiB.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Disk {
    pub free_mb: u64,
}

/// Process ids in use and the limit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Pids {
    pub current: u64,
    pub max: u64,
}

/// Reads the machine. `None` means "cannot read this here".
pub trait Probe {
    fn memory(&self) -> Option<Mem>;
    fn load(&self) -> Option<Load>;
    fn disk(&self, path: &Path) -> Option<Disk>;
    fn pids(&self) -> Option<Pids>;
    /// Checks that run here, but less precisely than they should (for
    /// example memory counting reclaimable page cache as used).
    fn notes(&self) -> Vec<String> {
        Vec::new()
    }
}

/// What the machine looks like to one spawn decision.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Census {
    /// Delegated children live on this machine, across every goal and
    /// session.
    pub live_machine: u32,
    /// Of those, children started recently that may not have ramped up
    /// yet: memory is reserved for each of them.
    pub ramping: u32,
    /// When the last child was started (Unix seconds), if ever.
    pub last_start: Option<u64>,
    /// Now (Unix seconds).
    pub now: u64,
}

/// The decision for one spawn.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Admit {
    Go,
    /// Queue the spawn; `reason` is shown (`queued: …`).
    Wait {
        reason: String,
    },
}

/// Delegation's resource limits. Defaults are in [`Thresholds::default`].
#[derive(Debug, Clone, PartialEq)]
pub struct Thresholds {
    /// Memory that must stay free after a new child is reserved.
    pub min_free_memory_mb: u64,
    /// Memory reserved for each child started but not yet ramped up,
    /// including the one being admitted.
    pub child_memory_mb: u64,
    /// Highest one-minute load per CPU at which a child may start.
    pub max_load_per_cpu: f64,
    /// Free space that must remain on each checked filesystem (the data
    /// dir, the workspace, the worktree dir).
    pub min_free_disk_mb: u64,
    /// Percent of the process-id limit that must stay free.
    pub min_free_pids_percent: u64,
    /// Least gap between two starts, so the load can register.
    pub spawn_interval_secs: u64,
    /// Delegated children live at once on the machine, every goal and
    /// session together.
    pub machine_max_live: u32,
    /// Niceness given to a delegated child (0 leaves it unchanged).
    pub child_nice: u32,
}

/// The CPUs this process may use (at least 1).
pub fn cpus() -> u32 {
    std::thread::available_parallelism()
        .map(|n| n.get() as u32)
        .unwrap_or(1)
}

impl Default for Thresholds {
    fn default() -> Self {
        Self {
            min_free_memory_mb: 1024,
            child_memory_mb: 512,
            max_load_per_cpu: 1.5,
            min_free_disk_mb: 2048,
            min_free_pids_percent: 10,
            spawn_interval_secs: 2,
            machine_max_live: cpus().max(2),
            child_nice: 10,
        }
    }
}

/// Optional overrides of [`Thresholds`], as a config file gives them.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Overrides {
    pub min_free_memory_mb: Option<u64>,
    pub child_memory_mb: Option<u64>,
    pub max_load_per_cpu: Option<f64>,
    pub min_free_disk_mb: Option<u64>,
    pub min_free_pids_percent: Option<u64>,
    pub spawn_interval_secs: Option<u64>,
    pub machine_max_live: Option<u32>,
    pub child_nice: Option<u32>,
}

/// Bounds of each key: `(name, least, most)`. Zero is never allowed for a
/// limit (it would disable the guard or admit nothing); the upper bounds
/// catch typos (a value in bytes for MiB, say).
const MB_MAX: u64 = 16 * 1024 * 1024; // 16 TiB
const BOUNDS: [(&str, u64, u64); 6] = [
    ("min_free_memory_mb", 1, MB_MAX),
    ("child_memory_mb", 1, MB_MAX),
    ("min_free_disk_mb", 1, MB_MAX),
    ("min_free_pids_percent", 1, 90),
    ("spawn_interval_secs", 1, 3600),
    ("machine_max_live", 1, 1024),
];
/// Largest `max_load_per_cpu`.
pub const MAX_LOAD_PER_CPU: f64 = 64.0;
/// Largest `child_nice` (the POSIX maximum).
pub const MAX_CHILD_NICE: u32 = 19;

impl Thresholds {
    /// The defaults with `o` applied; an out-of-bounds value is an error
    /// naming the `delegation.<key>`.
    pub fn with(o: &Overrides) -> Result<Self, String> {
        let ints = [
            o.min_free_memory_mb,
            o.child_memory_mb,
            o.min_free_disk_mb,
            o.min_free_pids_percent,
            o.spawn_interval_secs,
            o.machine_max_live.map(u64::from),
        ];
        for ((key, least, most), v) in BOUNDS.iter().zip(ints) {
            if let Some(v) = v {
                if v < *least || v > *most {
                    return Err(format!(
                        "delegation.{key} must be {least} to {most}, got {v}"
                    ));
                }
            }
        }
        if let Some(l) = o.max_load_per_cpu {
            if !(l.is_finite() && l > 0.0 && l <= MAX_LOAD_PER_CPU) {
                return Err(format!(
                    "delegation.max_load_per_cpu must be greater than 0 and at most \
                     {MAX_LOAD_PER_CPU}, got {l}"
                ));
            }
        }
        if let Some(n) = o.child_nice {
            if n > MAX_CHILD_NICE {
                return Err(format!(
                    "delegation.child_nice must be 0 to {MAX_CHILD_NICE}, got {n}"
                ));
            }
        }
        let d = Self::default();
        Ok(Self {
            min_free_memory_mb: o.min_free_memory_mb.unwrap_or(d.min_free_memory_mb),
            child_memory_mb: o.child_memory_mb.unwrap_or(d.child_memory_mb),
            max_load_per_cpu: o.max_load_per_cpu.unwrap_or(d.max_load_per_cpu),
            min_free_disk_mb: o.min_free_disk_mb.unwrap_or(d.min_free_disk_mb),
            min_free_pids_percent: o.min_free_pids_percent.unwrap_or(d.min_free_pids_percent),
            spawn_interval_secs: o.spawn_interval_secs.unwrap_or(d.spawn_interval_secs),
            machine_max_live: o.machine_max_live.unwrap_or(d.machine_max_live),
            child_nice: o.child_nice.unwrap_or(d.child_nice),
        })
    }

    /// May a delegated child start now? Checks, in order: the machine-wide
    /// live count, the spawn interval, memory (with `child_memory_mb`
    /// reserved for the new child and each ramping one), load, process ids
    /// and the free space of every path. The first check that fails is the
    /// reason to wait; a probe that cannot be read is skipped.
    pub fn admit(&self, probe: &dyn Probe, census: Census, paths: &[&Path]) -> Admit {
        let wait = |reason: String| Admit::Wait {
            reason: format!("queued: {reason}"),
        };
        if census.live_machine >= self.machine_max_live {
            return wait(format!(
                "{} delegated children live on this machine (max {})",
                census.live_machine, self.machine_max_live
            ));
        }
        if let Some(last) = census.last_start {
            let gap = census.now.saturating_sub(last);
            if gap < self.spawn_interval_secs {
                return wait(format!(
                    "last child started {gap}s ago (spawn interval {}s)",
                    self.spawn_interval_secs
                ));
            }
        }
        if let Some(m) = probe.memory() {
            let reserved = self
                .child_memory_mb
                .saturating_mul(u64::from(census.ramping) + 1);
            let needed = self.min_free_memory_mb.saturating_add(reserved);
            if m.available_mb < needed {
                return wait(format!(
                    "memory {} MB free < {needed} MB needed",
                    m.available_mb
                ));
            }
        }
        if let Some(l) = probe.load() {
            let per_cpu = l.one_min / f64::from(l.cpus.max(1));
            if per_cpu > self.max_load_per_cpu {
                return wait(format!(
                    "load {per_cpu:.2} per CPU > {:.2}",
                    self.max_load_per_cpu
                ));
            }
        }
        if let Some(p) = probe.pids() {
            let free = p.max.saturating_sub(p.current);
            // free / max < percent / 100, in integers.
            if p.max > 0
                && free.saturating_mul(100) < self.min_free_pids_percent.saturating_mul(p.max)
            {
                return wait(format!(
                    "process ids {} of {} in use (< {}% free)",
                    p.current, p.max, self.min_free_pids_percent
                ));
            }
        }
        for path in paths {
            if let Some(d) = probe.disk(path) {
                if d.free_mb < self.min_free_disk_mb {
                    return wait(format!(
                        "disk {} MB free on {} < {} MB",
                        d.free_mb,
                        path.display(),
                        self.min_free_disk_mb
                    ));
                }
            }
        }
        Admit::Go
    }
}

/// One startup note naming the checks `probe` cannot read here (skipped,
/// not failed); empty when every check can run.
pub fn startup_notes(probe: &dyn Probe, paths: &[&Path]) -> Vec<String> {
    let mut missing = Vec::new();
    if probe.memory().is_none() {
        missing.push("memory");
    }
    if probe.load().is_none() {
        missing.push("load");
    }
    if probe.pids().is_none() {
        missing.push("process ids");
    }
    if paths.iter().any(|p| probe.disk(p).is_none()) {
        missing.push("disk");
    }
    let mut notes = Vec::new();
    if !missing.is_empty() {
        notes.push(format!(
            "delegation resource guard: cannot read {} here; those checks are skipped",
            missing.join(", ")
        ));
    }
    notes.extend(
        probe
            .notes()
            .into_iter()
            .map(|n| format!("delegation resource guard: {n}")),
    );
    notes
}

// ------------------------------------------------------------ Linux probes

const MIB: u64 = 1024 * 1024;

/// `MemAvailable` of `/proc/meminfo`, in MiB.
pub fn parse_meminfo(text: &str) -> Option<u64> {
    text.lines().find_map(|l| {
        let rest = l.strip_prefix("MemAvailable:")?;
        let kb: u64 = rest.trim().strip_suffix("kB")?.trim().parse().ok()?;
        Some(kb / 1024)
    })
}

/// A cgroup v2 limit file (`memory.max`, `pids.max`): `Some(None)` for
/// `max` (no limit), `Some(Some(n))` for a number, `None` when unreadable.
pub fn parse_cgroup_limit(text: &str) -> Option<Option<u64>> {
    match text.trim() {
        "max" => Some(None),
        n => n.parse().ok().map(Some),
    }
}

/// A cgroup v2 counter file (`memory.current`, `pids.current`).
pub fn parse_cgroup_counter(text: &str) -> Option<u64> {
    text.trim().parse().ok()
}

/// The unified (v2) cgroup path of `/proc/self/cgroup` (`0::/path`).
pub fn parse_self_cgroup(text: &str) -> Option<String> {
    text.lines()
        .find_map(|l| l.strip_prefix("0::"))
        .map(|p| p.trim().to_string())
}

/// The one-minute load of `/proc/loadavg`.
pub fn parse_loadavg(text: &str) -> Option<f64> {
    text.split_whitespace().next()?.parse().ok()
}

/// Reclaimable page cache of a cgroup v2 `memory.stat`, in bytes:
/// `inactive_file`, else `file` when the kernel has no `inactive_file`.
pub fn parse_memory_stat_inactive_file(text: &str) -> Option<u64> {
    let field = |name: &str| {
        text.lines().find_map(|l| {
            let mut it = l.split_whitespace();
            if it.next()? != name {
                return None;
            }
            it.next()?.parse::<u64>().ok()
        })
    };
    field("inactive_file").or_else(|| field("file"))
}

/// Memory headroom: the cgroup's (limit − working set) when it has a limit,
/// else the host's `MemAvailable`; the smaller when both are known. The
/// working set is `memory.current` minus the reclaimable page cache
/// (`reclaimable`), as Kubernetes counts it: after heavy file IO
/// `memory.current` sits near the limit while most of it is cache the
/// kernel frees on demand. With `None` all of `memory.current` counts
/// (conservative).
pub fn memory_headroom(
    meminfo_mb: Option<u64>,
    cgroup: Option<(Option<u64>, u64)>,
    reclaimable: Option<u64>,
) -> Option<u64> {
    let cg = cgroup.and_then(|(limit, current)| {
        let working_set = current.saturating_sub(reclaimable.unwrap_or(0));
        limit.map(|l| l.saturating_sub(working_set) / MIB)
    });
    match (meminfo_mb, cg) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (a, b) => a.or(b),
    }
}

/// The note when a cgroup memory limit is set but its `memory.stat` gives
/// no page cache figure, so headroom falls back to (limit − current).
pub const MEMORY_STAT_NOTE: &str =
    "cannot read the cgroup memory.stat; memory headroom counts reclaimable page cache as used";

/// Memory headroom in MiB from the raw texts of `/proc/meminfo` and the
/// cgroup's `memory.max`, `memory.current` and `memory.stat`, plus
/// [`MEMORY_STAT_NOTE`] when the conservative fallback was taken.
pub fn memory_from_files(
    meminfo: Option<&str>,
    max: Option<&str>,
    current: Option<&str>,
    stat: Option<&str>,
) -> (Option<u64>, Option<&'static str>) {
    let meminfo = meminfo.and_then(parse_meminfo);
    let cgroup = max
        .and_then(parse_cgroup_limit)
        .zip(current.and_then(parse_cgroup_counter));
    let limited = matches!(cgroup, Some((Some(_), _)));
    let reclaimable = stat.and_then(parse_memory_stat_inactive_file);
    let note = (limited && reclaimable.is_none()).then_some(MEMORY_STAT_NOTE);
    (memory_headroom(meminfo, cgroup, reclaimable), note)
}

/// Probes the running Linux machine (cgroup v2 and `/proc`).
#[derive(Debug, Clone, Default)]
pub struct LinuxProbe;

impl LinuxProbe {
    fn memory_read() -> (Option<u64>, Option<&'static str>) {
        let meminfo = std::fs::read_to_string("/proc/meminfo").ok();
        let max = Self::cgroup_file("memory.max");
        let current = Self::cgroup_file("memory.current");
        let stat = Self::cgroup_file("memory.stat");
        memory_from_files(
            meminfo.as_deref(),
            max.as_deref(),
            current.as_deref(),
            stat.as_deref(),
        )
    }

    fn cgroup_file(name: &str) -> Option<String> {
        let rel = std::fs::read_to_string("/proc/self/cgroup")
            .ok()
            .and_then(|t| parse_self_cgroup(&t))
            .unwrap_or_default();
        let dir = Path::new("/sys/fs/cgroup").join(rel.trim_start_matches('/'));
        std::fs::read_to_string(dir.join(name))
            .or_else(|_| std::fs::read_to_string(Path::new("/sys/fs/cgroup").join(name)))
            .ok()
    }
}

impl Probe for LinuxProbe {
    fn memory(&self) -> Option<Mem> {
        Self::memory_read()
            .0
            .map(|available_mb| Mem { available_mb })
    }

    fn load(&self) -> Option<Load> {
        let one_min = std::fs::read_to_string("/proc/loadavg")
            .ok()
            .and_then(|t| parse_loadavg(&t))?;
        Some(Load {
            one_min,
            cpus: cpus(),
        })
    }

    fn disk(&self, path: &Path) -> Option<Disk> {
        statvfs_free(path).map(|bytes| Disk {
            free_mb: bytes / MIB,
        })
    }

    fn pids(&self) -> Option<Pids> {
        let cg_max = Self::cgroup_file("pids.max").and_then(|t| parse_cgroup_limit(&t));
        let cg_cur = Self::cgroup_file("pids.current").and_then(|t| parse_cgroup_counter(&t));
        if let (Some(Some(max)), Some(current)) = (cg_max, cg_cur) {
            return Some(Pids { current, max });
        }
        let max = std::fs::read_to_string("/proc/sys/kernel/pid_max")
            .ok()
            .and_then(|t| parse_cgroup_counter(&t))?;
        let current = std::fs::read_dir("/proc")
            .ok()?
            .flatten()
            .filter(|e| {
                e.file_name()
                    .to_str()
                    .is_some_and(|n| n.bytes().all(|b| b.is_ascii_digit()))
            })
            // pid_max bounds thread ids too: count each process's threads
            // (`/proc/<pid>/task`), one when that cannot be read.
            .map(|e| {
                std::fs::read_dir(e.path().join("task"))
                    .map(|t| t.count().max(1) as u64)
                    .unwrap_or(1)
            })
            .sum();
        Some(Pids { current, max })
    }

    fn notes(&self) -> Vec<String> {
        Self::memory_read()
            .1
            .map(|n| vec![n.to_string()])
            .unwrap_or_default()
    }
}

/// Bytes an unprivileged user may still write on `path`'s filesystem
/// (`f_bavail`, not `f_bfree`, which counts the root reserve).
#[cfg(unix)]
pub fn statvfs_free(path: &Path) -> Option<u64> {
    use std::os::unix::ffi::OsStrExt;
    let c = std::ffi::CString::new(path.as_os_str().as_bytes()).ok()?;
    let mut s: libc::statvfs = unsafe { std::mem::zeroed() };
    // SAFETY: `c` is a valid NUL-terminated path, `s` a valid out-pointer.
    if unsafe { libc::statvfs(c.as_ptr(), &mut s) } != 0 {
        return None;
    }
    Some((s.f_bavail as u64).saturating_mul(s.f_frsize as u64))
}

#[cfg(not(unix))]
pub fn statvfs_free(_path: &Path) -> Option<u64> {
    None
}

// ---------------------------------------------------------------- niceness

/// Start `cmd` with niceness at least `nice` (unix; elsewhere a no-op). A
/// child already nicer keeps its niceness, and a failure to change it never
/// stops the child from starting.
pub fn apply_child_nice(cmd: &mut std::process::Command, nice: u32) {
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        if nice == 0 {
            return;
        }
        let nice = nice.min(MAX_CHILD_NICE) as libc::c_int;
        // SAFETY: getpriority/setpriority are async-signal-safe; nothing
        // else runs between fork and exec.
        unsafe {
            cmd.pre_exec(move || {
                let current = libc::getpriority(libc::PRIO_PROCESS, 0);
                let target = current.max(nice);
                let _ = libc::setpriority(libc::PRIO_PROCESS, 0, target);
                Ok(())
            });
        }
    }
    #[cfg(not(unix))]
    {
        let _ = (cmd, nice);
    }
}

#[cfg(test)]
#[path = "resource_guard_tests.rs"]
mod tests;
