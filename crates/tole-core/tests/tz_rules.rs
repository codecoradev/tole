//! Time zones (#580, part 1): zone rules from TZif data or a POSIX TZ
//! string, wall-clock to instant mapping across DST, and replaceable
//! sources of zone data. The schedules that use them follow in part 2.

use tole_core::cron::days_from_civil;
use tole_core::tz::{self, Local, OffsetOnly, TzError, Zone, ZoneSource, ZoneinfoDir};

/// Unix seconds for a UTC date and time.
fn utc(y: i64, mo: u32, d: u32, h: i64, mi: i64) -> i64 {
    days_from_civil(y, mo, d) * 86_400 + h * 3_600 + mi * 60
}

fn new_york() -> Zone {
    Zone::posix("America/New_York", "EST5EDT,M3.2.0,M11.1.0").unwrap()
}

#[test]
fn offsets_follow_the_rule_around_both_changes() {
    let z = new_york();
    // 2026-03-08 07:00Z: 02:00 EST becomes 03:00 EDT.
    assert_eq!(z.offset_at(utc(2026, 3, 8, 6, 59)), -5 * 3_600);
    assert_eq!(z.offset_at(utc(2026, 3, 8, 7, 0)), -4 * 3_600);
    // 2026-11-01 06:00Z: 02:00 EDT becomes 01:00 EST.
    assert_eq!(z.offset_at(utc(2026, 11, 1, 5, 59)), -4 * 3_600);
    assert_eq!(z.offset_at(utc(2026, 11, 1, 6, 0)), -5 * 3_600);
    // Southern hemisphere: DST spans the new year.
    let syd = Zone::posix("Australia/Sydney", "AEST-10AEDT,M10.1.0,M4.1.0/3").unwrap();
    assert_eq!(syd.offset_at(utc(2026, 1, 15, 0, 0)), 11 * 3_600);
    assert_eq!(syd.offset_at(utc(2026, 7, 15, 0, 0)), 10 * 3_600);
}

#[test]
fn local_times_map_to_one_instant_a_gap_or_two_instants() {
    let z = new_york();
    let local = |y, mo, d, h, mi| utc(y, mo, d, h, mi); // a wall time as seconds
    assert_eq!(
        z.from_local(local(2026, 6, 1, 9, 0)),
        Local::Single(utc(2026, 6, 1, 13, 0))
    );
    // 02:30 on 2026-03-08 does not exist: the gap ends at 07:00Z.
    assert_eq!(
        z.from_local(local(2026, 3, 8, 2, 30)),
        Local::Gap(utc(2026, 3, 8, 7, 0))
    );
    // 01:30 on 2026-11-01 happens at 05:30Z (EDT) and 06:30Z (EST).
    assert_eq!(
        z.from_local(local(2026, 11, 1, 1, 30)),
        Local::Ambiguous(utc(2026, 11, 1, 5, 30), utc(2026, 11, 1, 6, 30))
    );
}

/// A TZif v2 file: `transitions` of `(at, type)`, `types` of
/// `(utoff, isdst)`, and a POSIX footer.
fn tzif(transitions: &[(i64, u8)], types: &[(i32, u8)], footer: &str) -> Vec<u8> {
    let header = |v: u8| {
        let mut h = b"TZif".to_vec();
        h.push(v);
        h.extend([0u8; 15]);
        for n in [0, 0, 0, transitions.len(), types.len(), 4] {
            h.extend((n as u32).to_be_bytes());
        }
        h
    };
    let mut out = header(b'2');
    // v1 block (32-bit), as real files carry it.
    for (at, _) in transitions {
        out.extend((*at as i32).to_be_bytes());
    }
    let body = |out: &mut Vec<u8>| {
        out.extend(transitions.iter().map(|(_, i)| *i));
        for (off, dst) in types {
            out.extend(off.to_be_bytes());
            out.extend([*dst, 0]);
        }
        out.extend(b"ZZZ\0");
    };
    body(&mut out);
    out.extend(header(b'2'));
    for (at, _) in transitions {
        out.extend(at.to_be_bytes());
    }
    body(&mut out);
    out.extend(format!("\n{footer}\n").bytes());
    out
}

#[test]
fn tzif_transitions_then_the_footer_rule() {
    // Two historical transitions, then the US rule from the footer.
    let bytes = tzif(
        &[(utc(2000, 4, 2, 7, 0), 1), (utc(2000, 10, 29, 6, 0), 0)],
        &[(-5 * 3_600, 0), (-4 * 3_600, 1)],
        "EST5EDT,M3.2.0,M11.1.0",
    );
    let z = Zone::from_tzif("America/New_York", &bytes).unwrap();
    assert_eq!(
        z.offset_at(utc(1999, 6, 1, 0, 0)),
        -5 * 3_600,
        "before the first"
    );
    assert_eq!(
        z.offset_at(utc(2000, 6, 1, 0, 0)),
        -4 * 3_600,
        "a listed one"
    );
    assert_eq!(z.offset_at(utc(2026, 7, 1, 0, 0)), -4 * 3_600, "the footer");
    assert_eq!(
        z.offset_at(utc(2026, 12, 1, 0, 0)),
        -5 * 3_600,
        "the footer"
    );
}

#[test]
fn broken_tzif_data_is_an_error_never_a_panic() {
    let good = tzif(
        &[(utc(2000, 4, 2, 7, 0), 1)],
        &[(-5 * 3_600, 0), (-4 * 3_600, 1)],
        "EST5EDT,M3.2.0,M11.1.0",
    );
    for cut in 0..good.len() {
        let _ = Zone::from_tzif("X", &good[..cut]);
    }
    let mut seed = 0x9e37_79b9_7f4a_7c15u64;
    for _ in 0..2_000 {
        let mut b = good.clone();
        for _ in 0..4 {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            let i = (seed as usize) % b.len();
            b[i] = (seed >> 32) as u8;
        }
        if let Ok(z) = Zone::from_tzif("X", &b) {
            let _ = z.offset_at(utc(2026, 1, 1, 0, 0));
            let _ = z.from_local(utc(2026, 3, 8, 2, 30));
        }
    }
    for (bad, why) in [
        (b"NOPE".to_vec(), "not TZif"),
        (tzif(&[(0, 5)], &[(0, 0)], ""), "type index"),
        (tzif(&[], &[(30 * 3_600, 0)], ""), "offset"),
        (tzif(&[(10, 0), (5, 0)], &[(0, 0)], ""), "order"),
        (tzif(&[], &[(0, 0)], "EST5EDT,M13.2.0,M11.1.0"), "footer"),
    ] {
        assert!(Zone::from_tzif("X", &bad).is_err(), "{why}");
    }
}

#[test]
fn names_and_offsets_are_validated_with_a_remedy() {
    assert_eq!(tz::resolve(&OffsetOnly, "UTC").unwrap(), Zone::utc());
    assert_eq!(
        tz::resolve(&OffsetOnly, "-03:30").unwrap().offset_at(0),
        -(3 * 3_600 + 30 * 60)
    );
    for bad in ["+15:00", "+7", "+07:60", "07:00", "", "utc+1"] {
        let e: TzError = tz::resolve(&OffsetOnly, bad).unwrap_err();
        assert!(e.to_string().contains("+07:00"), "{bad}: {e}");
    }
    // An IANA name with the offset-only source names the source and the fix.
    let e = tz::resolve(&OffsetOnly, "Asia/Jakarta")
        .unwrap_err()
        .to_string();
    assert!(e.contains("offset-only") && e.contains("system"), "{e}");
    // Names that could leave the zoneinfo directory never reach the disk.
    let dir = ZoneinfoDir::new(std::env::temp_dir());
    for bad in [
        "../etc/passwd",
        "Asia/../../x",
        "/etc/passwd",
        "a//b",
        ".hidden",
    ] {
        let e = tz::resolve(&dir, bad).unwrap_err().to_string();
        assert!(e.contains("not a valid zone name"), "{bad}: {e}");
    }
    let e = tz::resolve(&ZoneinfoDir::new("/nonexistent-tz".into()), "Mars/Olympus")
        .unwrap_err()
        .to_string();
    assert!(
        e.contains("Mars/Olympus") && e.contains("/nonexistent-tz"),
        "{e}"
    );
}

/// A zoneinfo directory follows its own alias links, but never one that
/// leads out of it, and a FIFO is refused without blocking.
#[cfg(unix)]
#[test]
fn a_zoneinfo_dir_stays_inside_itself_and_never_blocks() {
    let root = std::env::temp_dir().join(format!("tole-tz-dir-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(root.join("Asia")).unwrap();
    let jakarta = tzif(&[], &[(7 * 3_600, 0)], "<+07>-7");
    std::fs::write(root.join("Asia/Jakarta"), &jakarta).unwrap();
    std::os::unix::fs::symlink("Jakarta", root.join("Asia/Batavia")).unwrap();
    let outside = root.with_extension("outside");
    std::fs::write(&outside, &jakarta).unwrap();
    std::os::unix::fs::symlink(&outside, root.join("Asia/Out")).unwrap();
    let made = std::process::Command::new("mkfifo")
        .arg(root.join("Asia/Fifo"))
        .status()
        .unwrap();
    assert!(made.success());
    let dir = ZoneinfoDir::new(root.clone());
    assert_eq!(
        tz::resolve(&dir, "Asia/Batavia").unwrap().offset_at(0),
        7 * 3_600
    );
    let e = tz::resolve(&dir, "Asia/Out").unwrap_err().to_string();
    assert!(e.contains("outside"), "{e}");
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(tz::resolve(&dir, "Asia/Fifo").map(|_| ()));
    });
    let r = rx
        .recv_timeout(std::time::Duration::from_secs(5))
        .expect("never blocks");
    assert!(r.is_err());
}

#[test]
fn the_system_zoneinfo_agrees_with_the_rule_when_present() {
    let dir = ZoneinfoDir::new("/usr/share/zoneinfo".into());
    let Ok(z) = tz::resolve(&dir, "America/New_York") else {
        eprintln!("no system zoneinfo here; skipped");
        return;
    };
    let rule = new_york();
    for k in 0..400 {
        let t = utc(2025, 1, 1, 0, 0) + k * 86_400 + 3_600 * (k % 24);
        assert_eq!(z.offset_at(t), rule.offset_at(t), "at {t}");
    }
}

/// A source that serves fixed TZif bytes for one name.
#[derive(Debug)]
struct OneZone(&'static str, Vec<u8>);

impl ZoneSource for OneZone {
    fn label(&self) -> String {
        "test".into()
    }
    fn tzif(&self, name: &str) -> Result<Vec<u8>, TzError> {
        if name == self.0 {
            Ok(self.1.clone())
        } else {
            Err(TzError(format!("no zone `{name}` in test")))
        }
    }
}

/// The source is replaceable: any `ZoneSource` serves IANA names, while
/// UTC, fixed offsets and name validation stay the same for every source.
#[test]
fn a_custom_source_serves_names_and_its_errors_pass_through() {
    let src = OneZone("Asia/Jakarta", tzif(&[], &[(7 * 3_600, 0)], "<+07>-7"));
    let z = tz::resolve(&src, "Asia/Jakarta").unwrap();
    assert_eq!((z.name(), z.offset_at(0)), ("Asia/Jakarta", 7 * 3_600));
    assert_eq!(
        z.from_local(utc(2026, 1, 1, 9, 0)),
        Local::Single(utc(2026, 1, 1, 2, 0))
    );
    let e = tz::resolve(&src, "Asia/Tokyo").unwrap_err().to_string();
    assert!(e.contains("Asia/Tokyo"), "{e}");
    assert!(tz::resolve(&src, "+05:30").is_ok());
    assert!(tz::resolve(&src, "../x").is_err());
}
