//! Per-job time zones (#580, part 2): cron on a zone's wall clock with
//! defined DST behavior, and the ledger that schedules jobs in their zone.
//!
//! DST rule (design §4, Hermes-compatible): a local time that does not
//! exist (spring forward) fires once at the first instant after the gap; a
//! local time that happens twice (fall back) fires once, on its first
//! occurrence. `@every` is a plain duration and ignores the zone. A job
//! without a time zone keeps plain UTC math and writes the same `fire`
//! record as before.

use std::sync::Arc;

use serde_json::Map;
use tole_core::cron::{days_from_civil, Schedule, Zoned};
use tole_core::ledger::{CatchUp, Job, Lease, Ledger, MemoryStore};
use tole_core::tz::{self, OffsetOnly, TzError, Zone, ZoneSource};

/// Unix seconds for a UTC date and time.
fn utc(y: i64, mo: u32, d: u32, h: i64, mi: i64) -> i64 {
    days_from_civil(y, mo, d) * 86_400 + h * 3_600 + mi * 60
}

fn new_york() -> Zone {
    Zone::posix("America/New_York", "EST5EDT,M3.2.0,M11.1.0").unwrap()
}

fn zoned(expr: &str, zone: Zone) -> Zoned {
    Zoned {
        schedule: Schedule::parse(expr).unwrap(),
        zone,
    }
}

/// The next `n` occurrences after `after`.
fn walk(z: &Zoned, mut after: i64, n: usize) -> Vec<i64> {
    let mut out = Vec::new();
    for _ in 0..n {
        let Some(t) = z.next_after(after, 0) else {
            break;
        };
        out.push(t);
        after = t;
    }
    out
}

/// Table: schedule, start, expected occurrences (UTC).
#[test]
fn wall_clock_schedules_across_dst_fire_at_most_once() {
    let ny = new_york();
    let cases: Vec<(&str, i64, Vec<i64>)> = vec![
        // 09:00 local stays 09:00 local across the change.
        (
            "0 9 * * *",
            utc(2026, 3, 7, 0, 0),
            vec![utc(2026, 3, 7, 14, 0), utc(2026, 3, 8, 13, 0)],
        ),
        // A skipped 02:30 fires once at the end of the gap, then as usual.
        (
            "30 2 * * *",
            utc(2026, 3, 7, 12, 0),
            vec![utc(2026, 3, 8, 7, 0), utc(2026, 3, 9, 6, 30)],
        ),
        // A repeated 01:30 fires once, on its first occurrence.
        (
            "30 1 * * *",
            utc(2026, 10, 31, 12, 0),
            vec![utc(2026, 11, 1, 5, 30), utc(2026, 11, 2, 6, 30)],
        ),
        // Every half hour over the spring gap: 02:00 and 02:30 collapse
        // into the one instant 03:00 EDT, which is not repeated.
        (
            "*/30 * * * *",
            utc(2026, 3, 8, 6, 15),
            vec![
                utc(2026, 3, 8, 6, 30),
                utc(2026, 3, 8, 7, 0),
                utc(2026, 3, 8, 7, 30),
            ],
        ),
        // Every half hour over the fall overlap: the repeated hour is not
        // run a second time.
        (
            "*/30 * * * *",
            utc(2026, 11, 1, 4, 45),
            vec![
                utc(2026, 11, 1, 5, 0),
                utc(2026, 11, 1, 5, 30),
                utc(2026, 11, 1, 7, 0),
            ],
        ),
        // Starting inside the repeated hour (a job saved at 01:10 EST, the
        // second pass): 01:30 already happened at 05:30Z and is not run again.
        (
            "*/30 * * * *",
            utc(2026, 11, 1, 6, 10),
            vec![utc(2026, 11, 1, 7, 0)],
        ),
    ];
    for (expr, start, want) in cases {
        let z = zoned(expr, ny.clone());
        assert_eq!(walk(&z, start, want.len()), want, "{expr} from {start}");
        // The span over the same window counts each occurrence once.
        let span = z.span(start, *want.last().unwrap(), 0).unwrap();
        assert_eq!(span.count, want.len() as u64, "{expr}");
        assert_eq!((span.first, span.last), (want[0], *want.last().unwrap()));
    }
}

#[test]
fn a_fire_records_its_dst_case() {
    let z = zoned("30 2 * * *", new_york());
    assert_eq!(z.dst_case(utc(2026, 3, 8, 7, 0)), Some("gap"));
    let z = zoned("30 1 * * *", new_york());
    assert_eq!(z.dst_case(utc(2026, 11, 1, 5, 30)), Some("first_of_two"));
    assert_eq!(z.dst_case(utc(2026, 11, 2, 6, 30)), None);
}

#[test]
fn fixed_offsets_utc_and_every_need_no_rules() {
    let z = zoned("0 9 * * *", tz::resolve(&OffsetOnly, "+07:00").unwrap());
    assert_eq!(
        walk(&z, utc(2026, 1, 1, 0, 0), 1),
        vec![utc(2026, 1, 1, 2, 0)]
    );
    // UTC behaves exactly like a schedule without a zone.
    let s = Schedule::parse("*/7 3-5 * * 1-5").unwrap();
    let u = zoned("*/7 3-5 * * 1-5", Zone::utc());
    for k in 0..500 {
        let t = utc(2026, 1, 1, 0, 0) + k * 9_973;
        assert_eq!(u.next_after(t, 0), s.next_after(t, 0));
    }
    // `@every` is a duration: the zone changes nothing.
    let e = zoned("@every 1h", new_york());
    let anchor = utc(2026, 3, 8, 5, 0);
    assert_eq!(
        e.next_after(utc(2026, 3, 8, 6, 30), anchor),
        Some(utc(2026, 3, 8, 7, 0))
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

fn job(id: &str, tz: Option<&str>) -> Job {
    Job {
        id: id.into(),
        schedule: "0 9 * * *".into(),
        timezone: tz.map(str::to_string),
        catch_up: CatchUp::Skip,
        grace_minutes: 0,
        paused: false,
        removed: false,
        at: utc(2026, 1, 1, 0, 0),
        extra: Map::new(),
    }
}

#[test]
fn the_ledger_schedules_a_job_in_its_zone_and_records_it() {
    let jakarta = OneZone("Asia/Jakarta", tzif(&[], &[(7 * 3_600, 0)], "<+07>-7"));
    let mut l = Ledger::open_with_zones(MemoryStore::default(), Arc::new(jakarta)).unwrap();
    l.put_job(job("jkt", Some("Asia/Jakarta"))).unwrap();
    l.put_job(job("plain", None)).unwrap();
    assert_eq!(l.state().next_run("jkt"), Some(utc(2026, 1, 1, 2, 0)));
    assert_eq!(l.state().next_run("plain"), Some(utc(2026, 1, 1, 9, 0)));
    let lease = Lease {
        pid: 1,
        start_time: 1,
    };
    let at = utc(2026, 1, 1, 2, 0);
    let due = l.ready(at).unwrap();
    let fire = l.claim(&due[0], lease, at).unwrap().unwrap();
    let z = fire.tz.expect("a zoned job records its zone");
    assert_eq!(
        (z.zone.as_str(), z.offset, z.case),
        ("Asia/Jakarta", 7 * 3_600, None)
    );
    // A job without a zone writes the same fire record as before.
    let at = utc(2026, 1, 1, 9, 0);
    let due = l.ready(at).unwrap();
    let plain = due.iter().find(|d| d.job_id == "plain").unwrap();
    let fire = l.claim(plain, lease, at).unwrap().unwrap();
    assert!(fire.tz.is_none());
    let line = String::from_utf8(l.store().bytes()).unwrap();
    let last = line.lines().last().unwrap();
    assert!(!last.contains("\"tz\""), "{last}");
}

#[test]
fn an_unknown_zone_is_refused_on_save_naming_the_source() {
    let mut l = Ledger::open(MemoryStore::default()).unwrap();
    let e = l
        .put_job(job("x", Some("Asia/Jakarta")))
        .unwrap_err()
        .to_string();
    assert!(
        e.contains("Asia/Jakarta") && e.contains("offset-only"),
        "{e}"
    );
    // The default source still takes UTC and fixed offsets.
    l.put_job(job("o", Some("+07:00"))).unwrap();
    l.put_job(job("u", Some("UTC"))).unwrap();
    assert_eq!(l.state().next_run("o"), Some(utc(2026, 1, 1, 2, 0)));
}
