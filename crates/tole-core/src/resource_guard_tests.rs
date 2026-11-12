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
    let bad: Vec<(Overrides, &str)> = vec![
        (
            Overrides {
                min_free_memory_mb: Some(0),
                ..Default::default()
            },
            "min_free_memory_mb",
        ),
        (
            Overrides {
                child_memory_mb: Some(MB_MAX + 1),
                ..Default::default()
            },
            "child_memory_mb",
        ),
        (
            Overrides {
                min_free_disk_mb: Some(0),
                ..Default::default()
            },
            "min_free_disk_mb",
        ),
        (
            Overrides {
                min_free_pids_percent: Some(0),
                ..Default::default()
            },
            "min_free_pids_percent",
        ),
        (
            Overrides {
                min_free_pids_percent: Some(91),
                ..Default::default()
            },
            "min_free_pids_percent",
        ),
        (
            Overrides {
                spawn_interval_secs: Some(0),
                ..Default::default()
            },
            "spawn_interval_secs",
        ),
        (
            Overrides {
                spawn_interval_secs: Some(3601),
                ..Default::default()
            },
            "spawn_interval_secs",
        ),
        (
            Overrides {
                machine_max_live: Some(0),
                ..Default::default()
            },
            "machine_max_live",
        ),
        (
            Overrides {
                machine_max_live: Some(1025),
                ..Default::default()
            },
            "machine_max_live",
        ),
        (
            Overrides {
                max_load_per_cpu: Some(0.0),
                ..Default::default()
            },
            "max_load_per_cpu",
        ),
        (
            Overrides {
                max_load_per_cpu: Some(-1.0),
                ..Default::default()
            },
            "max_load_per_cpu",
        ),
        (
            Overrides {
                max_load_per_cpu: Some(f64::NAN),
                ..Default::default()
            },
            "max_load_per_cpu",
        ),
        (
            Overrides {
                max_load_per_cpu: Some(64.5),
                ..Default::default()
            },
            "max_load_per_cpu",
        ),
        (
            Overrides {
                child_nice: Some(20),
                ..Default::default()
            },
            "child_nice",
        ),
    ];
    for (o, key) in bad {
        let err = Thresholds::with(&o).unwrap_err();
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
        memory_headroom(Some(9216), Some((Some(1 << 30), 1 << 29))),
        Some(512)
    );
    // `memory.max` = max: no limit, the host's figure.
    assert_eq!(
        memory_headroom(Some(9216), Some((None, 1 << 29))),
        Some(9216)
    );
    // The smaller headroom wins either way.
    assert_eq!(
        memory_headroom(Some(100), Some((Some(1 << 30), 0))),
        Some(100)
    );
    assert_eq!(memory_headroom(None, Some((Some(1 << 30), 0))), Some(1024));
    // Over the limit: zero, never an underflow.
    assert_eq!(memory_headroom(None, Some((Some(1), 5))), Some(0));
    assert_eq!(memory_headroom(None, None), None);
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
