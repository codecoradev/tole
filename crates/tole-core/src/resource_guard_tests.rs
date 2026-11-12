use super::*;
use std::path::PathBuf;

/// A probe with fixed answers; `disk` maps a path to its free MiB.
#[derive(Default)]
struct Fixed {
    mem: Option<u64>,
    load: Option<(f64, u32)>,
    disk: Vec<(PathBuf, u64)>,
    pids: Option<(u64, u64)>,
}

impl Probe for Fixed {
    fn memory(&self) -> Option<Mem> {
        self.mem.map(|available_mb| Mem { available_mb })
    }
    fn load(&self) -> Option<Load> {
        self.load.map(|(one_min, cpus)| Load { one_min, cpus })
    }
    fn disk(&self, path: &Path) -> Option<Disk> {
        self.disk
            .iter()
            .find(|(p, _)| p == path)
            .map(|(_, free_mb)| Disk { free_mb: *free_mb })
    }
    fn pids(&self) -> Option<Pids> {
        self.pids.map(|(current, max)| Pids { current, max })
    }
}

/// Room for everything under the default thresholds.
fn roomy() -> Fixed {
    Fixed {
        mem: Some(64 * 1024),
        load: Some((0.1, 4)),
        disk: vec![
            (PathBuf::from("/data"), 100_000),
            (PathBuf::from("/ws"), 100_000),
        ],
        pids: Some((100, 10_000)),
    }
}

fn t() -> Thresholds {
    Thresholds {
        machine_max_live: 4,
        ..Thresholds::default()
    }
}

const NOW: u64 = 1_000_000;

fn census() -> Census {
    Census {
        now: NOW,
        ..Census::default()
    }
}

fn paths() -> Vec<PathBuf> {
    vec![PathBuf::from("/data"), PathBuf::from("/ws")]
}

fn admit(th: &Thresholds, probe: &Fixed, c: Census) -> Admit {
    let p = paths();
    let refs: Vec<&Path> = p.iter().map(PathBuf::as_path).collect();
    th.admit(probe, c, &refs)
}

fn reason(a: Admit) -> String {
    match a {
        Admit::Wait { reason } => reason,
        Admit::Go => panic!("expected Wait, got Go"),
    }
}

#[test]
fn defaults_follow_the_owner_requirement() {
    let d = Thresholds::default();
    assert_eq!(d.min_free_memory_mb, 1024);
    assert_eq!(d.child_memory_mb, 512);
    assert_eq!(d.max_load_per_cpu, 1.5);
    assert_eq!(d.min_free_disk_mb, 2048);
    assert_eq!(d.min_free_pids_percent, 10);
    assert_eq!(d.spawn_interval_secs, 2);
    assert_eq!(d.machine_max_live, cpus().max(2));
    assert_eq!(d.child_nice, 10);
    assert_eq!(Thresholds::with(&Overrides::default()).unwrap(), d);
}

#[test]
fn a_machine_with_room_admits() {
    assert_eq!(admit(&t(), &roomy(), census()), Admit::Go);
}

#[test]
fn memory_needs_the_floor_plus_the_new_child() {
    // 1024 floor + 512 for the new child = 1536.
    let at = Fixed {
        mem: Some(1536),
        ..roomy()
    };
    assert_eq!(admit(&t(), &at, census()), Admit::Go);
    let below = Fixed {
        mem: Some(1535),
        ..roomy()
    };
    assert_eq!(
        reason(admit(&t(), &below, census())),
        "queued: memory 1535 MB free < 1536 MB needed"
    );
}

/// A child that started but has not ramped up yet has not used its memory:
/// it is reserved, so a second start right after the first waits.
#[test]
fn a_ramping_child_reserves_its_memory() {
    let probe = Fixed {
        mem: Some(1800),
        ..roomy()
    };
    assert_eq!(admit(&t(), &probe, census()), Admit::Go);
    let second = Census {
        live_machine: 1,
        ramping: 1,
        ..census()
    };
    assert_eq!(
        reason(admit(&t(), &probe, second)),
        "queued: memory 1800 MB free < 2048 MB needed"
    );
    // Once it has ramped (it shows in the free memory itself), no reserve.
    let ramped = Census {
        live_machine: 1,
        ramping: 0,
        ..census()
    };
    assert_eq!(admit(&t(), &probe, ramped), Admit::Go);
}

#[test]
fn load_per_cpu_is_bounded() {
    // 6.0 over 4 CPUs = 1.5: at the limit, admitted.
    let at = Fixed {
        load: Some((6.0, 4)),
        ..roomy()
    };
    assert_eq!(admit(&t(), &at, census()), Admit::Go);
    let over = Fixed {
        load: Some((6.04, 4)),
        ..roomy()
    };
    assert_eq!(
        reason(admit(&t(), &over, census())),
        "queued: load 1.51 per CPU > 1.50"
    );
    // A reported 0 CPUs counts as one, never a division by zero.
    let zero = Fixed {
        load: Some((1.6, 0)),
        ..roomy()
    };
    assert!(matches!(admit(&t(), &zero, census()), Admit::Wait { .. }));
}

#[test]
fn every_checked_filesystem_needs_free_space() {
    let at = Fixed {
        disk: vec![(PathBuf::from("/data"), 2048), (PathBuf::from("/ws"), 2048)],
        ..roomy()
    };
    assert_eq!(admit(&t(), &at, census()), Admit::Go);
    // The second path is the full one: it is checked too.
    let full = Fixed {
        disk: vec![
            (PathBuf::from("/data"), 100_000),
            (PathBuf::from("/ws"), 2047),
        ],
        ..roomy()
    };
    assert_eq!(
        reason(admit(&t(), &full, census())),
        "queued: disk 2047 MB free on /ws < 2048 MB"
    );
}

#[test]
fn process_ids_keep_a_free_share() {
    // 10% of 1000 must stay free: 900 in use is the limit.
    let at = Fixed {
        pids: Some((900, 1000)),
        ..roomy()
    };
    assert_eq!(admit(&t(), &at, census()), Admit::Go);
    let over = Fixed {
        pids: Some((901, 1000)),
        ..roomy()
    };
    assert_eq!(
        reason(admit(&t(), &over, census())),
        "queued: process ids 901 of 1000 in use (< 10% free)"
    );
    let no_limit = Fixed {
        pids: Some((5, 0)),
        ..roomy()
    };
    assert_eq!(admit(&t(), &no_limit, census()), Admit::Go);
}

#[test]
fn starts_are_spaced_by_the_spawn_interval() {
    let just = Census {
        last_start: Some(NOW - 1),
        ..census()
    };
    assert_eq!(
        reason(admit(&t(), &roomy(), just)),
        "queued: last child started 1s ago (spawn interval 2s)"
    );
    let spaced = Census {
        last_start: Some(NOW - 2),
        ..census()
    };
    assert_eq!(admit(&t(), &roomy(), spaced), Admit::Go);
    // A clock that went backwards waits rather than racing.
    let future = Census {
        last_start: Some(NOW + 5),
        ..census()
    };
    assert!(matches!(admit(&t(), &roomy(), future), Admit::Wait { .. }));
}

/// The live count is the machine's, every goal and session together.
#[test]
fn the_machine_wide_live_count_is_bounded() {
    let three = Census {
        live_machine: 3,
        ..census()
    };
    assert_eq!(admit(&t(), &roomy(), three), Admit::Go);
    let four = Census {
        live_machine: 4,
        ..census()
    };
    assert_eq!(
        reason(admit(&t(), &roomy(), four)),
        "queued: 4 delegated children live on this machine (max 4)"
    );
}

/// An unreadable probe skips its check (one startup note), it neither
/// blocks forever nor counts as plenty for the checks that can run.
#[test]
fn unreadable_probes_are_skipped_with_one_note() {
    let blind = Fixed::default();
    assert_eq!(admit(&t(), &blind, census()), Admit::Go);
    let p = paths();
    let refs: Vec<&Path> = p.iter().map(PathBuf::as_path).collect();
    let notes = startup_notes(&blind, &refs);
    assert_eq!(notes.len(), 1);
    assert_eq!(
        notes[0],
        "delegation resource guard: cannot read memory, load, process ids, disk here; \
         those checks are skipped"
    );
    assert!(startup_notes(&roomy(), &refs).is_empty());
    // Memory unreadable, disk full: the disk still decides.
    let partial = Fixed {
        mem: None,
        disk: vec![
            (PathBuf::from("/data"), 10),
            (PathBuf::from("/ws"), 100_000),
        ],
        ..roomy()
    };
    assert!(reason(admit(&t(), &partial, census())).contains("disk 10 MB free on /data"));
    // The non-counted rules still apply without any probe.
    let busy = Census {
        live_machine: 4,
        ..census()
    };
    assert!(matches!(admit(&t(), &blind, busy), Admit::Wait { .. }));
}

#[test]
fn overrides_are_bounded() {
    let ok = Thresholds::with(&Overrides {
        min_free_memory_mb: Some(1),
        child_memory_mb: Some(MB_MAX),
        max_load_per_cpu: Some(MAX_LOAD_PER_CPU),
        min_free_disk_mb: Some(1),
        min_free_pids_percent: Some(90),
        spawn_interval_secs: Some(3600),
        machine_max_live: Some(1024),
        child_nice: Some(0),
    })
    .unwrap();
    assert_eq!(ok.child_memory_mb, MB_MAX);
    assert_eq!(ok.child_nice, 0);
    // One override per case, by key name: (key, value).
    let one = |key: &str, v: f64| {
        let n = v as u64;
        let mut o = Overrides::default();
        match key {
            "min_free_memory_mb" => o.min_free_memory_mb = Some(n),
            "child_memory_mb" => o.child_memory_mb = Some(n),
            "min_free_disk_mb" => o.min_free_disk_mb = Some(n),
            "min_free_pids_percent" => o.min_free_pids_percent = Some(n),
            "spawn_interval_secs" => o.spawn_interval_secs = Some(n),
            "machine_max_live" => o.machine_max_live = Some(n as u32),
            "max_load_per_cpu" => o.max_load_per_cpu = Some(v),
            "child_nice" => o.child_nice = Some(n as u32),
            _ => unreachable!("{key}"),
        }
        o
    };
    let bad = [
        ("min_free_memory_mb", 0.0),
        ("child_memory_mb", (MB_MAX + 1) as f64),
        ("min_free_disk_mb", 0.0),
        ("min_free_pids_percent", 0.0),
        ("min_free_pids_percent", 91.0),
        ("spawn_interval_secs", 0.0),
        ("spawn_interval_secs", 3601.0),
        ("machine_max_live", 0.0),
        ("machine_max_live", 1025.0),
        ("max_load_per_cpu", 0.0),
        ("max_load_per_cpu", -1.0),
        ("max_load_per_cpu", f64::NAN),
        ("max_load_per_cpu", 64.5),
        ("child_nice", 20.0),
    ];
    for (key, v) in bad {
        let err = Thresholds::with(&one(key, v)).unwrap_err();
        assert!(err.contains(&format!("delegation.{key}")), "{err}");
    }
}

// ------------------------------------------------------------ parse fixtures

const MEMINFO: &str = "MemTotal:       16314216 kB\nMemFree:          812344 kB\n\
                       MemAvailable:    9437184 kB\nBuffers:          123456 kB\n";

#[test]
fn meminfo_gives_mem_available() {
    assert_eq!(parse_meminfo(MEMINFO), Some(9216));
    assert_eq!(parse_meminfo("MemTotal: 1 kB\n"), None);
    assert_eq!(parse_meminfo("MemAvailable: lots\n"), None);
}

#[test]
fn cgroup_v2_files_parse_limits_counters_and_paths() {
    assert_eq!(parse_cgroup_limit("max\n"), Some(None));
    assert_eq!(parse_cgroup_limit("1073741824\n"), Some(Some(1 << 30)));
    assert_eq!(parse_cgroup_limit("lots\n"), None);
    assert_eq!(parse_cgroup_counter("536870912\n"), Some(1 << 29));
    assert_eq!(parse_cgroup_counter(""), None);
    assert_eq!(
        parse_self_cgroup("0::/user.slice/user-1000.slice/session-2.scope\n").as_deref(),
        Some("/user.slice/user-1000.slice/session-2.scope")
    );
    // A hybrid host lists v1 controllers too; only `0::` is v2.
    assert_eq!(
        parse_self_cgroup("12:pids:/a\n4:memory:/b\n0::/c\n").as_deref(),
        Some("/c")
    );
    assert_eq!(parse_self_cgroup("12:pids:/a\n"), None);
}

#[test]
fn memory_headroom_takes_the_cgroup_limit_when_there_is_one() {
    // A container capped at 1 GiB using 512 MiB: 512 MiB left, even with
    // 9 GiB available on the host.
    assert_eq!(
        memory_headroom(Some(9216), Some((Some(1 << 30), 1 << 29)), None),
        Some(512)
    );
    // `memory.max` = max: no limit, the host's figure.
    assert_eq!(
        memory_headroom(Some(9216), Some((None, 1 << 29)), None),
        Some(9216)
    );
    // The smaller headroom wins either way.
    assert_eq!(
        memory_headroom(Some(100), Some((Some(1 << 30), 0)), None),
        Some(100)
    );
    assert_eq!(
        memory_headroom(None, Some((Some(1 << 30), 0)), None),
        Some(1024)
    );
    // Over the limit: zero, never an underflow.
    assert_eq!(memory_headroom(None, Some((Some(1), 5)), None), Some(0));
    assert_eq!(memory_headroom(None, None, None), None);
}

// A cgroup v2 memory.stat as the kernel writes it (trimmed).
const STAT_3G_INACTIVE: &str = "anon 734003200\nfile 3489660928\nkernel 52428800\nshmem 0\n\
active_anon 0\ninactive_anon 734003200\nactive_file 268435456\ninactive_file 3221225472\n\
unevictable 0\n";
const GIB: u64 = 1 << 30;

#[test]
fn memory_stat_gives_the_reclaimable_page_cache() {
    assert_eq!(
        parse_memory_stat_inactive_file(STAT_3G_INACTIVE),
        Some(3 * GIB)
    );
    // An older kernel without inactive_file: the file field.
    assert_eq!(
        parse_memory_stat_inactive_file("anon 1\nfile 4096\n"),
        Some(4096)
    );
    // A longer name is not the field; a garbled value is no value.
    assert_eq!(parse_memory_stat_inactive_file("inactive_file_x 5\n"), None);
    assert_eq!(parse_memory_stat_inactive_file("inactive_file x\n"), None);
    assert_eq!(parse_memory_stat_inactive_file(""), None);
}

#[test]
fn headroom_is_the_limit_minus_the_working_set() {
    let limit = (4 * GIB).to_string();
    let current = (39 * GIB / 10).to_string();
    let host = "MemTotal: 67108864 kB\nMemAvailable: 50331648 kB\n";
    // 4 GiB limit, 3.9 GiB current of which 3.0 GiB is inactive file cache:
    // a 0.9 GiB working set, about 3.1 GiB free, so a child starts.
    let (mb, note) = memory_from_files(
        Some(host),
        Some(&limit),
        Some(&current),
        Some(STAT_3G_INACTIVE),
    );
    let mb = mb.unwrap();
    assert!((3170..=3175).contains(&mb), "{mb}");
    assert_eq!(note, None);
    let probe = Fixed {
        mem: Some(mb),
        ..roomy()
    };
    assert_eq!(admit(&t(), &probe, census()), Admit::Go);
    // The same current with no inactive cache: about 102 MiB, so it waits.
    let stat0 = "anon 4187593113\nfile 0\ninactive_file 0\n";
    let (mb, note) = memory_from_files(Some(host), Some(&limit), Some(&current), Some(stat0));
    let mb = mb.unwrap();
    assert!(mb < 110, "{mb}");
    assert_eq!(note, None);
    let probe = Fixed {
        mem: Some(mb),
        ..roomy()
    };
    assert!(reason(admit(&t(), &probe, census())).contains("memory"));
}

struct Noting;
impl Probe for Noting {
    fn memory(&self) -> Option<Mem> {
        roomy().memory()
    }
    fn load(&self) -> Option<Load> {
        roomy().load()
    }
    fn disk(&self, p: &Path) -> Option<Disk> {
        roomy().disk(p)
    }
    fn pids(&self) -> Option<Pids> {
        roomy().pids()
    }
    fn notes(&self) -> Vec<String> {
        vec![MEMORY_STAT_NOTE.to_string()]
    }
}

#[test]
fn an_unreadable_memory_stat_falls_back_to_limit_minus_current_with_a_note() {
    let limit = (4 * GIB).to_string();
    let current = (39 * GIB / 10).to_string();
    for stat in [None, Some("garbage\n"), Some("inactive_file lots\n")] {
        let (mb, note) = memory_from_files(None, Some(&limit), Some(&current), stat);
        assert!(mb.unwrap() < 110, "{stat:?}");
        assert_eq!(note, Some(MEMORY_STAT_NOTE), "{stat:?}");
    }
    // No cgroup limit: MemAvailable alone, and memory.stat is not needed.
    let (mb, note) = memory_from_files(
        Some("MemAvailable: 2097152 kB\n"),
        Some("max\n"),
        Some(&current),
        None,
    );
    assert_eq!((mb, note), (Some(2048), None));
    // The note reaches the startup notes.
    let notes = startup_notes(&Noting, &[]);
    assert_eq!(notes.len(), 1, "{notes:?}");
    assert!(notes[0].contains("memory.stat"), "{notes:?}");
}

#[test]
fn a_huge_pid_limit_does_not_overflow() {
    let mut th = t();
    th.min_free_pids_percent = 90;
    let probe = Fixed {
        pids: Some((0, u64::MAX)),
        ..roomy()
    };
    assert_eq!(admit(&th, &probe, census()), Admit::Go);
}

#[test]
fn loadavg_gives_the_one_minute_load() {
    assert_eq!(parse_loadavg("0.52 0.58 0.59 1/432 12345\n"), Some(0.52));
    assert_eq!(parse_loadavg(""), None);
    assert_eq!(parse_loadavg("high 0.5"), None);
}

/// The real probe reads this Linux machine (the sandbox is one).
#[cfg(target_os = "linux")]
#[test]
fn the_linux_probe_reads_this_machine() {
    let p = LinuxProbe;
    assert!(p.memory().is_some_and(|m| m.available_mb > 0));
    assert!(p.load().is_some_and(|l| l.cpus >= 1 && l.one_min >= 0.0));
    let tmp = std::env::temp_dir();
    assert!(p.disk(&tmp).is_some());
    assert!(p.disk(Path::new("/no/such/dir/at/all")).is_none());
    assert!(p.pids().is_some_and(|x| x.max > 0 && x.current > 0));
}

/// The 19th field of a `/proc/<pid>/stat` line (niceness).
#[cfg(target_os = "linux")]
fn nice_of(stat: &str) -> i64 {
    let after = &stat[stat.rfind(')').unwrap() + 2..];
    after.split_whitespace().nth(16).unwrap().parse().unwrap()
}

#[cfg(target_os = "linux")]
#[test]
fn a_child_runs_at_least_at_the_child_niceness() {
    let ours = nice_of(&std::fs::read_to_string("/proc/self/stat").unwrap());
    let mut cmd = std::process::Command::new("cat");
    cmd.arg("/proc/self/stat");
    apply_child_nice(&mut cmd, 10);
    let out = cmd.output().unwrap();
    let child = nice_of(&String::from_utf8_lossy(&out.stdout));
    assert_eq!(child, ours.max(10));
    // 0 leaves it unchanged.
    let mut plain = std::process::Command::new("cat");
    plain.arg("/proc/self/stat");
    apply_child_nice(&mut plain, 0);
    let out = plain.output().unwrap();
    assert_eq!(nice_of(&String::from_utf8_lossy(&out.stdout)), ours);
}
