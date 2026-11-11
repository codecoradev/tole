//! Schedule expressions for the automation ledger (issue #578).
//!
//! A small in-tree parser for 5-field cron and `@every <duration>`, plus the
//! next-occurrence math. Pure: times are Unix seconds passed in by the
//! caller, nothing reads a clock. **UTC only** for now; time zones (#580)
//! will add a zone parameter to [`Schedule::next_after`] — the expression
//! text and the ledger format do not change.
//!
//! Grammar (`minute hour day-of-month month day-of-week`):
//! - each field is a comma list of items; an item is `*`, `N`, `A-B`, `*/S`
//!   or `A-B/S` (`N/S` is rejected as ambiguous — write `N-MAX/S`);
//! - minute 0–59, hour 0–23, day-of-month 1–31, month 1–12 or `jan`–`dec`,
//!   day-of-week 0–7 or `sun`–`sat` (0 and 7 are Sunday), case-insensitive;
//! - macros `@hourly @daily @midnight @weekly @monthly @yearly @annually`;
//! - `@every 1h30m` (also `every 1h30m`): units `d h m s`, at least 60 s.
//!
//! **Day rule (Vixie cron / cronie):** a day-of-month or day-of-week field
//! whose text starts with `*` (so `*` and `*/2`) is "unrestricted". When
//! either day field is unrestricted, a day matches if BOTH fields match;
//! when both are restricted, a day matches if EITHER matches
//! (`0 0 13 * 5` = every 13th and every Friday).
//!
//! An expression that can never fire (`0 0 30 2 *`) is rejected at parse
//! time: the Gregorian calendar repeats every 400 years (146 097 days, a
//! whole number of weeks), so searching one cycle is a complete check.

/// Seconds per day.
const DAY: i64 = 86_400;
/// One full Gregorian cycle in days (also a multiple of 7).
const CYCLE_DAYS: i64 = 146_097;
/// Latest instant schedules reason about: 9999-12-31T23:59:59Z.
pub const MAX_TIME: i64 = 253_402_300_799;
/// Earliest instant: 0000-03-01 is far enough back for any caller.
pub const MIN_TIME: i64 = -62_162_035_200;

/// A schedule expression that failed validation (with the exact reason).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("invalid schedule `{expr}`: {reason}")]
pub struct ScheduleError {
    /// The rejected expression text.
    pub expr: String,
    /// What is wrong, naming the field and value.
    pub reason: String,
}

/// A parsed schedule.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Schedule {
    /// 5-field cron.
    Cron(Cron),
    /// A fixed period in seconds, counted from the job's anchor.
    Every(i64),
}

/// A parsed 5-field cron expression (bit sets per field).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cron {
    minutes: u64,
    hours: u32,
    dom: u32,
    months: u16,
    dow: u8,
    dom_star: bool,
    dow_star: bool,
}

/// The owed occurrences in a window: see [`Schedule::span`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Span {
    /// Earliest occurrence in the window.
    pub first: i64,
    /// Latest occurrence in the window.
    pub last: i64,
    /// The occurrence just before `last` in the window, if any.
    pub before_last: Option<i64>,
    /// Number of occurrences in the window.
    pub count: u64,
}

impl Schedule {
    /// Parses and validates an expression (see the module docs).
    pub fn parse(expr: &str) -> Result<Self, ScheduleError> {
        let err = |reason: String| ScheduleError {
            expr: expr.to_string(),
            reason,
        };
        let text = expr.trim();
        if text.len() > 256 {
            return Err(err("longer than 256 characters".into()));
        }
        let lower = text.to_ascii_lowercase();
        if let Some(dur) = lower
            .strip_prefix("@every ")
            .or_else(|| lower.strip_prefix("every "))
        {
            return parse_duration(dur.trim()).map(Schedule::Every).map_err(err);
        }
        let body = match lower.as_str() {
            "@hourly" => "0 * * * *",
            "@daily" | "@midnight" => "0 0 * * *",
            "@weekly" => "0 0 * * 0",
            "@monthly" => "0 0 1 * *",
            "@yearly" | "@annually" => "0 0 1 1 *",
            s if s.starts_with('@') => return Err(err(format!("unknown macro `{s}`"))),
            s => s,
        };
        let cron = Cron::parse(body).map_err(err)?;
        // Reject expressions that never fire (validated on save, P5).
        if cron.next_after(0).is_none() {
            return Err(err(
                "never fires (day-of-month does not exist in the chosen months)".into(),
            ));
        }
        Ok(Schedule::Cron(cron))
    }

    /// First occurrence strictly after `after` (Unix seconds, UTC).
    /// `anchor` is where an `@every` period is counted from (occurrences
    /// are `anchor + k * period`, `k >= 1`); cron ignores it. `None` when
    /// no occurrence exists before [`MAX_TIME`].
    pub fn next_after(&self, after: i64, anchor: i64) -> Option<i64> {
        match self {
            Schedule::Cron(c) => c.next_after(after),
            Schedule::Every(p) => {
                let k = if after < anchor {
                    1
                } else {
                    (after - anchor) / p + 1
                };
                anchor
                    .checked_add(k.checked_mul(*p)?)
                    .filter(|t| *t <= MAX_TIME)
            }
        }
    }

    /// The occurrences in `(after, until]`, or `None` when there are none.
    /// Linear in the number of occurrences for cron, O(1) for `@every`.
    pub fn span(&self, after: i64, until: i64, anchor: i64) -> Option<Span> {
        let first = self.next_after(after, anchor).filter(|t| *t <= until)?;
        if let Schedule::Every(p) = self {
            let count = (until - first) / p + 1;
            let last = first + (count - 1) * p;
            let before_last = (count > 1).then(|| last - p);
            return Some(Span {
                first,
                last,
                before_last,
                count: count as u64,
            });
        }
        let (mut last, mut before_last, mut count) = (first, None, 1u64);
        while let Some(t) = self.next_after(last, anchor).filter(|t| *t <= until) {
            before_last = Some(last);
            last = t;
            count += 1;
        }
        Some(Span {
            first,
            last,
            before_last,
            count,
        })
    }
}

impl Cron {
    fn parse(body: &str) -> Result<Self, String> {
        let fields: Vec<&str> = body.split_whitespace().collect();
        if fields.len() != 5 {
            return Err(format!(
                "expected 5 fields (minute hour day-of-month month day-of-week), found {}",
                fields.len()
            ));
        }
        let minutes = parse_field(fields[0], &MINUTE)?;
        let hours = parse_field(fields[1], &HOUR)? as u32;
        let dom = parse_field(fields[2], &DOM)? as u32;
        let months = parse_field(fields[3], &MONTH)? as u16;
        let mut dow = parse_field(fields[4], &DOW)?;
        if dow & (1 << 7) != 0 {
            dow = (dow | 1) & 0x7f; // 7 is Sunday
        }
        Ok(Cron {
            minutes,
            hours,
            dom,
            months,
            dow: dow as u8,
            dom_star: fields[2].starts_with('*'),
            dow_star: fields[4].starts_with('*'),
        })
    }

    /// True when the minute `(month, dom, weekday, hour, minute)` matches.
    pub fn matches(&self, month: u32, dom: u32, weekday: u32, hour: u32, minute: u32) -> bool {
        self.months & (1 << month) != 0
            && self.day_matches(dom, weekday)
            && self.hours & (1 << hour) != 0
            && self.minutes & (1 << minute) != 0
    }

    fn day_matches(&self, dom: u32, weekday: u32) -> bool {
        let a = self.dom & (1 << dom) != 0;
        let b = self.dow & (1 << weekday) != 0;
        if self.dom_star || self.dow_star {
            a && b
        } else {
            a || b
        }
    }

    /// First matching minute strictly after `after`, within one 400-year
    /// cycle (after that, nothing will ever match).
    fn next_after(&self, after: i64) -> Option<i64> {
        if !(MIN_TIME..MAX_TIME).contains(&after) {
            return None;
        }
        let start = after.div_euclid(60) * 60 + 60;
        let mut day = start.div_euclid(DAY);
        let mut minute_of_day = (start.rem_euclid(DAY) / 60) as u32;
        let limit = day + CYCLE_DAYS;
        while day <= limit {
            let (y, m, d) = civil_from_days(day);
            if self.months & (1 << m) == 0 {
                let (ny, nm) = if m == 12 { (y + 1, 1) } else { (y, m + 1) };
                day = days_from_civil(ny, nm, 1);
                minute_of_day = 0;
                continue;
            }
            if self.day_matches(d, weekday(day)) {
                if let Some(t) = self.first_time_from(minute_of_day) {
                    let at = day * DAY + i64::from(t) * 60;
                    return (at <= MAX_TIME).then_some(at);
                }
            }
            day += 1;
            minute_of_day = 0;
        }
        None
    }

    /// First matching minute-of-day at or after `from`.
    fn first_time_from(&self, from: u32) -> Option<u32> {
        let (mut h, mut m) = (from / 60, from % 60);
        while h < 24 {
            if self.hours & (1 << h) != 0 {
                let rest = self.minutes & (u64::MAX << m);
                if rest != 0 {
                    return Some(h * 60 + rest.trailing_zeros());
                }
            }
            h += 1;
            m = 0;
        }
        None
    }
}

struct FieldSpec {
    name: &'static str,
    min: u32,
    max: u32,
    names: &'static [&'static str],
}

const MINUTE: FieldSpec = FieldSpec {
    name: "minute",
    min: 0,
    max: 59,
    names: &[],
};
const HOUR: FieldSpec = FieldSpec {
    name: "hour",
    min: 0,
    max: 23,
    names: &[],
};
const DOM: FieldSpec = FieldSpec {
    name: "day-of-month",
    min: 1,
    max: 31,
    names: &[],
};
const MONTH: FieldSpec = FieldSpec {
    name: "month",
    min: 1,
    max: 12,
    names: &[
        "jan", "feb", "mar", "apr", "may", "jun", "jul", "aug", "sep", "oct", "nov", "dec",
    ],
};
const DOW: FieldSpec = FieldSpec {
    name: "day-of-week",
    min: 0,
    max: 7,
    names: &["sun", "mon", "tue", "wed", "thu", "fri", "sat"],
};

fn parse_field(text: &str, spec: &FieldSpec) -> Result<u64, String> {
    let mut bits = 0u64;
    for item in text.split(',') {
        let err = |why: String| format!("{} field `{text}`: {why}", spec.name);
        let (range, step) = match item.split_once('/') {
            Some((r, s)) => {
                let step: u32 = s
                    .parse()
                    .map_err(|_| err(format!("step `{s}` is not a number")))?;
                if step == 0 || step > spec.max {
                    return Err(err(format!("step {step} out of range 1-{}", spec.max)));
                }
                (r, Some(step))
            }
            None => (item, None),
        };
        let (lo, hi) = if range == "*" {
            (spec.min, spec.max)
        } else if let Some((a, b)) = range.split_once('-') {
            let (lo, hi) = (value(a, spec).map_err(err)?, value(b, spec).map_err(err)?);
            if lo > hi {
                return Err(err(format!("range {lo}-{hi} is reversed")));
            }
            (lo, hi)
        } else {
            if step.is_some() {
                return Err(err(format!(
                    "`{item}` is ambiguous; write `{range}-{}/STEP`",
                    spec.max
                )));
            }
            let v = value(range, spec).map_err(err)?;
            (v, v)
        };
        let step = step.unwrap_or(1);
        let mut v = lo;
        while v <= hi {
            bits |= 1 << v;
            v += step;
        }
    }
    Ok(bits)
}

fn value(text: &str, spec: &FieldSpec) -> Result<u32, String> {
    if let Some(i) = spec.names.iter().position(|n| *n == text) {
        return Ok(i as u32 + spec.min);
    }
    let v: u32 = text.parse().map_err(|_| {
        if text.is_empty() {
            "empty value".to_string()
        } else {
            format!("`{text}` is not a number or name")
        }
    })?;
    if v < spec.min || v > spec.max {
        return Err(format!("value {v} out of range {}-{}", spec.min, spec.max));
    }
    Ok(v)
}

fn parse_duration(text: &str) -> Result<i64, String> {
    if text.is_empty() {
        return Err("`@every` needs a duration like `30m` or `1h30m`".into());
    }
    let (mut total, mut num) = (0i64, String::new());
    for c in text.chars() {
        if c.is_ascii_digit() {
            num.push(c);
            continue;
        }
        let unit = match c {
            'd' => DAY,
            'h' => 3_600,
            'm' => 60,
            's' => 1,
            _ => {
                return Err(format!(
                    "duration `{text}`: unknown unit `{c}` (use d, h, m, s)"
                ))
            }
        };
        if num.is_empty() {
            return Err(format!("duration `{text}`: unit `{c}` has no number"));
        }
        let n: i64 = num
            .parse()
            .map_err(|_| format!("duration `{text}` is longer than 366 days"))?;
        total = n
            .checked_mul(unit)
            .and_then(|v| v.checked_add(total))
            .filter(|t| *t <= 366 * DAY)
            .ok_or_else(|| format!("duration `{text}` is longer than 366 days"))?;
        num.clear();
    }
    if !num.is_empty() {
        return Err(format!("duration `{text}`: number `{num}` has no unit"));
    }
    if total < 60 {
        return Err(format!("duration `{text}` is shorter than 60 seconds"));
    }
    Ok(total)
}

/// Days since 1970-01-01 for a proleptic Gregorian date (H. Hinnant).
pub fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = i64::from((m + 9) % 12);
    let doy = (153 * mp + 2) / 5 + i64::from(d) - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * CYCLE_DAYS + doe - 719_468
}

/// `(year, month, day)` for days since 1970-01-01 (H. Hinnant).
pub fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(CYCLE_DAYS);
    let doe = z - era * CYCLE_DAYS;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let y = yoe + era * 400 + i64::from(m <= 2);
    (y, m, d)
}

/// Day of week for days since 1970-01-01; 0 = Sunday (1970-01-01 was a Thursday).
pub fn weekday(days: i64) -> u32 {
    (days + 4).rem_euclid(7) as u32
}
