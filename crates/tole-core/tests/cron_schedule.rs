//! Cron / `@every` schedule math (issue #578): table tests for parsing and
//! day semantics, plus a seeded property test of `next_after` against a
//! brute-force minute walker that keeps its own calendar (independent of
//! the civil-date conversion under test).

use tole_core::cron::{civil_from_days, days_from_civil, weekday, Schedule};

/// 2026-01-01T00:00:00Z.
const T2026: i64 = 1_767_225_600;

fn at(y: i64, mo: u32, d: u32, h: i64, mi: i64) -> i64 {
    days_from_civil(y, mo, d) * 86_400 + h * 3_600 + mi * 60
}

fn next(expr: &str, after: i64) -> i64 {
    Schedule::parse(expr).unwrap().next_after(after, 0).unwrap()
}

#[test]
fn civil_conversion_known_dates() {
    assert_eq!(days_from_civil(1970, 1, 1), 0);
    assert_eq!(weekday(0), 4, "1970-01-01 was a Thursday");
    assert_eq!(
        days_from_civil(2000, 3, 1) - days_from_civil(2000, 2, 28),
        2
    );
    assert_eq!(
        days_from_civil(1900, 3, 1) - days_from_civil(1900, 2, 28),
        1
    );
    assert_eq!(
        days_from_civil(2100, 3, 1) - days_from_civil(2100, 2, 28),
        1
    );
    assert_eq!(
        days_from_civil(2024, 3, 1) - days_from_civil(2024, 2, 28),
        2
    );
    assert_eq!(at(2026, 1, 1, 0, 0), T2026);
    for d in -800_000..800_000i64 {
        if d % 997 == 0 {
            let (y, m, dd) = civil_from_days(d);
            assert_eq!(days_from_civil(y, m, dd), d);
        }
    }
}

#[test]
fn parse_rejects_malformed_with_precise_reason() {
    let cases = [
        ("* * * *", "expected 5 fields"),
        (
            "60 * * * *",
            "minute field `60`: value 60 out of range 0-59",
        ),
        ("* 24 * * *", "hour field"),
        (
            "* * 0 * *",
            "day-of-month field `0`: value 0 out of range 1-31",
        ),
        ("* * * 13 *", "month field"),
        ("* * * * 8", "day-of-week field"),
        ("5-2 * * * *", "range 5-2 is reversed"),
        ("*/0 * * * *", "step 0 out of range"),
        ("5/15 * * * *", "ambiguous"),
        ("a * * * *", "`a` is not a number or name"),
        ("1,,2 * * * *", "empty value"),
        ("0 0 30 2 *", "never fires"),
        ("0 0 31 4,6,9,11 *", "never fires"),
        ("@reboot", "unknown macro"),
        ("@every", "unknown macro"),
        ("@every 30", "has no unit"),
        ("@every 59s", "shorter than 60 seconds"),
        ("@every 1w", "unknown unit `w`"),
        ("@every 400d", "longer than 366 days"),
        ("@every 99999999999999999999h", "longer than 366 days"),
        ("@every h", "unit `h` has no number"),
    ];
    for (expr, want) in cases {
        let e = Schedule::parse(expr).expect_err(expr).to_string();
        assert!(e.contains(want), "{expr}: got `{e}`, want `{want}`");
    }
}

#[test]
fn parse_accepts_names_macros_and_every() {
    let monfri = Schedule::parse("0 9 * JAN-dec Mon-FRI").unwrap();
    assert_eq!(monfri, Schedule::parse("0 9 * 1-12 1-5").unwrap());
    assert_eq!(
        Schedule::parse("0 0 * * 7").unwrap(),
        Schedule::parse("0 0 * * sun").unwrap()
    );
    assert_eq!(
        Schedule::parse("@daily").unwrap(),
        Schedule::parse("0 0 * * *").unwrap()
    );
    assert_eq!(
        Schedule::parse("@every 1h30m").unwrap(),
        Schedule::Every(5_400)
    );
    assert_eq!(
        Schedule::parse("every 2d").unwrap(),
        Schedule::Every(172_800)
    );
    // 29 Feb exists only in leap years, so it is valid.
    assert_eq!(
        next("0 12 29 2 *", T2026),
        at(2028, 2, 29, 12, 0),
        "next leap day after 2026"
    );
    assert_eq!(
        next("0 0 29 2 *", at(2096, 3, 1, 0, 0)),
        at(2104, 2, 29, 0, 0)
    );
}

#[test]
fn day_of_month_and_day_of_week_follow_vixie_rule() {
    // Both restricted: OR. 2026-02-13 is a Friday; 2026-02-06 is a Friday.
    let start = at(2026, 2, 1, 0, 0);
    assert_eq!(next("0 0 13 * 5", start), at(2026, 2, 6, 0, 0));
    assert_eq!(
        next("0 0 13 * 5", at(2026, 2, 6, 0, 0)),
        at(2026, 2, 13, 0, 0)
    );
    assert_eq!(next("0 0 1 * 1", start), at(2026, 2, 2, 0, 0), "Monday 2nd");
    // Day-of-week starts with `*` (here `*/3` = Sun, Wed, Sat): AND.
    // Days 10-20 of Feb 2026 that are Sun/Wed/Sat: 11 (Wed).
    assert_eq!(next("0 0 10-20 * */3", start), at(2026, 2, 11, 0, 0));
    // Day-of-month `*/2` is unrestricted too: odd days AND Monday.
    // Mondays in Feb 2026: 2, 9, 16, 23 → first odd Monday is 9.
    assert_eq!(next("0 0 */2 * 1", start), at(2026, 2, 9, 0, 0));
    // Plain `*` day-of-month: only the weekday matters.
    assert_eq!(next("0 0 * * 2", start), at(2026, 2, 3, 0, 0));
}

#[test]
fn next_after_is_strict_and_handles_month_ends() {
    assert_eq!(next("0 * * * *", T2026), T2026 + 3_600);
    assert_eq!(next("* * * * *", T2026 + 59), T2026 + 60);
    assert_eq!(
        next("0 0 31 * *", at(2026, 1, 31, 0, 0)),
        at(2026, 3, 31, 0, 0)
    );
    assert_eq!(next("59 23 31 12 *", T2026), at(2026, 12, 31, 23, 59));
    assert_eq!(
        next("*/20 1-2 * * *", at(2026, 1, 1, 2, 40)),
        at(2026, 1, 2, 1, 0)
    );
    let every = Schedule::parse("@every 10m").unwrap();
    assert_eq!(every.next_after(T2026, T2026), Some(T2026 + 600));
    assert_eq!(every.next_after(T2026 - 5, T2026), Some(T2026 + 600));
    assert_eq!(every.next_after(T2026 + 1_199, T2026), Some(T2026 + 1_200));
    assert_eq!(every.next_after(T2026 + 1_200, T2026), Some(T2026 + 1_800));
}

#[test]
fn span_counts_owed_occurrences() {
    let s = Schedule::parse("*/15 * * * *").unwrap();
    let sp = s.span(T2026, T2026 + 3_600, 0).unwrap();
    assert_eq!(
        (sp.first, sp.last, sp.count),
        (T2026 + 900, T2026 + 3_600, 4)
    );
    assert_eq!(sp.before_last, Some(T2026 + 2_700));
    assert!(s.span(T2026, T2026 + 899, 0).is_none());
    let e = Schedule::parse("@every 15m").unwrap();
    let sp = e.span(T2026, T2026 + 3_600, T2026).unwrap();
    assert_eq!(
        (sp.first, sp.last, sp.count),
        (T2026 + 900, T2026 + 3_600, 4)
    );
    assert_eq!(sp.before_last, Some(T2026 + 2_700));
}

// ---- property test against a minute-walking oracle -----------------------

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        // xorshift64*
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }
    fn below(&mut self, n: u32) -> u32 {
        (self.next() % u64::from(n)) as u32
    }
}

fn random_field(r: &mut Rng, min: u32, max: u32) -> String {
    let item = |r: &mut Rng| -> String {
        let a = min + r.below(max - min + 1);
        let b = a + r.below(max - a + 1);
        match r.below(6) {
            0 => "*".into(),
            1 => format!("*/{}", 1 + r.below(max.min(12))),
            2 => format!("{a}"),
            3 => format!("{a}-{b}"),
            4 => format!("{a}-{b}/{}", 1 + r.below(4)),
            _ => format!("{a},{b}"),
        }
    };
    let n = 1 + r.below(3);
    (0..n).map(|_| item(r)).collect::<Vec<_>>().join(",")
}

fn days_in(y: i64, m: u32) -> u32 {
    match m {
        2 if (y % 4 == 0 && y % 100 != 0) || y % 400 == 0 => 29,
        2 => 28,
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    }
}

/// Walks minute by minute from `after` with its own calendar counters.
fn oracle(s: &Schedule, after: i64, limit_minutes: u32) -> Option<i64> {
    let Schedule::Cron(c) = s else { unreachable!() };
    let mut t = after.div_euclid(60) * 60;
    let day = t.div_euclid(86_400);
    let (mut y, mut mo, mut d) = civil_from_days(day);
    let mut wd = weekday(day);
    let mut mins = (t.rem_euclid(86_400) / 60) as u32;
    for _ in 0..limit_minutes {
        t += 60;
        mins += 1;
        if mins == 1_440 {
            mins = 0;
            wd = (wd + 1) % 7;
            d += 1;
            if d > days_in(y, mo) {
                d = 1;
                mo += 1;
                if mo > 12 {
                    mo = 1;
                    y += 1;
                }
            }
        }
        if c.matches(mo, d, wd, mins / 60, mins % 60) {
            return Some(t);
        }
    }
    None
}

#[test]
fn next_after_matches_minute_walking_oracle() {
    let mut r = Rng(0x5eed_0578);
    // Starts near leap days, century years and month ends, plus random.
    let anchors = [
        at(2024, 2, 27, 23, 58),
        at(2100, 2, 27, 0, 0),
        at(2000, 2, 28, 12, 0),
        at(2026, 12, 31, 23, 59),
        at(2027, 4, 30, 23, 30),
    ];
    const LIMIT: u32 = 120 * 1_440; // 120 days
    let mut checked = 0;
    for i in 0..400 {
        let expr = format!(
            "{} {} {} {} {}",
            random_field(&mut r, 0, 59),
            random_field(&mut r, 0, 23),
            random_field(&mut r, 1, 31),
            random_field(&mut r, 1, 12),
            random_field(&mut r, 0, 7),
        );
        let Ok(s) = Schedule::parse(&expr) else {
            continue; // e.g. reversed ranges are rejected by design
        };
        let start = if i % 3 == 0 {
            anchors[(i / 3) % anchors.len()] + i64::from(r.below(120))
        } else {
            T2026 + i64::from(r.below(400 * 86_400)) - 200 * 86_400
        };
        let got = s.next_after(start, 0);
        match oracle(&s, start, LIMIT) {
            Some(want) => assert_eq!(got, Some(want), "`{expr}` after {start}"),
            None => assert!(
                got.is_none_or(|g| g > start + i64::from(LIMIT) * 60),
                "`{expr}` after {start}: impl {got:?} but oracle found none in window"
            ),
        }
        // Chain a few occurrences to cover the strictly-after contract.
        if let Some(mut t) = got {
            for _ in 0..3 {
                let n = s.next_after(t, 0).unwrap();
                if n - t > i64::from(LIMIT) * 60 {
                    break;
                }
                assert_eq!(Some(n), oracle(&s, t, LIMIT), "`{expr}` after {t}");
                t = n;
            }
        }
        checked += 1;
    }
    assert!(checked > 300, "only {checked} expressions checked");
}
