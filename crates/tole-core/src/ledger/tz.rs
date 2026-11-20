//! Time zones for schedules (#580, design `docs/automation-design.md` §4).
//!
//! A [`Zone`] is UTC, a fixed offset, or rules: the transitions of a TZif
//! file (RFC 8536, read from bytes) followed by its POSIX TZ footer, or a
//! POSIX TZ string alone. No zone data is embedded: an IANA name is
//! resolved through a replaceable [`ZoneSource`]. [`OffsetOnly`] takes no
//! data at all, and [`ZoneinfoDir`] reads the host's zoneinfo directory at
//! run time. An embedder can plug in its own source (bundled data, a
//! database) without touching the scheduler.
//!
//! [`Zone::from_local`] maps a wall-clock time to the instants it names:
//! one, none (a gap: the result is the first instant after it), or two (an
//! overlap). Leap-second zones (`right/`) are refused. Pure apart from
//! [`ZoneinfoDir`], and never panics on bad data.

use std::io::Read;
use std::path::PathBuf;
use std::sync::Arc;

use crate::cron::{civil_from_days, days_from_civil, weekday};

const DAY: i64 = 86_400;
/// Largest UTC offset accepted, in seconds (real zones stay within ±15 h).
const MAX_OFFSET: i32 = 26 * 3_600;
/// Largest TZif file a [`ZoneinfoDir`] reads.
pub const MAX_TZIF_BYTES: u64 = 512 * 1024;

/// A zone that cannot be resolved or parsed, with the reason and a remedy.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct TzError(pub String);

/// A time zone: a name and its rules.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Zone {
    name: String,
    rules: Rules,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Rules {
    Fixed(i32),
    Table(Arc<Table>),
}

/// Transitions (`at[i]`: from then on the offset is `off[i]`), the offset
/// before the first one, and the rule after the last one.
#[derive(Debug, PartialEq, Eq)]
struct Table {
    initial: i32,
    at: Vec<i64>,
    off: Vec<i32>,
    tail: Option<Posix>,
}

/// What a wall-clock time names in a zone (all values Unix seconds).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Local {
    /// Exactly one instant.
    Single(i64),
    /// No instant (a spring-forward gap): the first instant after the gap.
    Gap(i64),
    /// Two instants (a fall-back overlap): the first and the second.
    Ambiguous(i64, i64),
}

impl Zone {
    /// UTC.
    pub fn utc() -> Self {
        Zone {
            name: "UTC".into(),
            rules: Rules::Fixed(0),
        }
    }

    /// The name the zone was resolved from.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// True for UTC (and `+00:00`): schedules then use plain UTC math.
    pub fn is_utc(&self) -> bool {
        self.rules == Rules::Fixed(0)
    }

    /// A zone from a POSIX TZ string such as `EST5EDT,M3.2.0,M11.1.0`.
    pub fn posix(name: &str, spec: &str) -> Result<Zone, TzError> {
        let tail = Posix::parse(spec)
            .map_err(|e| TzError(format!("zone `{name}`: TZ rule `{spec}`: {e}")))?;
        Ok(Zone {
            name: name.into(),
            rules: Rules::Table(Arc::new(Table {
                initial: tail.std,
                at: Vec::new(),
                off: Vec::new(),
                tail: Some(tail),
            })),
        })
    }

    /// A zone from TZif bytes (RFC 8536, versions 1 to 4).
    pub fn from_tzif(name: &str, bytes: &[u8]) -> Result<Zone, TzError> {
        let table = parse_tzif(bytes)
            .map_err(|e| TzError(format!("zone `{name}`: invalid TZif data ({e})")))?;
        Ok(Zone {
            name: name.into(),
            rules: Rules::Table(Arc::new(table)),
        })
    }

    /// The UTC offset in seconds (east positive) in effect at instant `t`.
    pub fn offset_at(&self, t: i64) -> i32 {
        match &self.rules {
            Rules::Fixed(o) => *o,
            Rules::Table(tb) => {
                let i = tb.at.partition_point(|&x| x <= t);
                match (i, &tb.tail) {
                    (i, Some(p)) if i == tb.at.len() => p.offset_at(t),
                    (0, _) => tb.initial,
                    (i, _) => tb.off[i - 1],
                }
            }
        }
    }

    /// The instants the wall-clock time `local` (seconds, as if UTC) names.
    pub fn from_local(&self, local: i64) -> Local {
        let mut found: Vec<i64> = [local.saturating_sub(DAY), local, local.saturating_add(DAY)]
            .iter()
            .map(|p| self.offset_at(*p))
            .filter_map(|o| {
                let t = local.checked_sub(i64::from(o))?;
                (self.offset_at(t) == o).then_some(t)
            })
            .collect();
        found.sort_unstable();
        found.dedup();
        match found.as_slice() {
            [t] => Local::Single(*t),
            [a, .., b] => Local::Ambiguous(*a, *b),
            [] => {
                // The gap ends at the transition between the offset before
                // (`ob`) and after (`oa`); it lies in (local - oa, local - ob].
                let ob = i64::from(self.offset_at(local.saturating_sub(DAY)));
                let oa = self.offset_at(local.saturating_add(DAY));
                let (mut lo, mut hi) = (
                    local.saturating_sub(i64::from(oa)),
                    local.saturating_sub(ob),
                );
                while hi - lo > 1 {
                    let mid = lo + (hi - lo) / 2;
                    if self.offset_at(mid) == oa {
                        hi = mid;
                    } else {
                        lo = mid;
                    }
                }
                Local::Gap(hi)
            }
        }
    }
}

/// A source of zone data for IANA names. Implement it to plug in bundled
/// data or any other store; [`resolve`] validates the name first.
pub trait ZoneSource: Send + Sync + std::fmt::Debug {
    /// How the source is named in errors, e.g. `system (/usr/share/zoneinfo)`.
    fn label(&self) -> String;
    /// The TZif data for a validated IANA name.
    fn tzif(&self, name: &str) -> Result<Vec<u8>, TzError>;
}

/// UTC and fixed offsets only: every IANA name is refused.
#[derive(Debug, Default, Clone, Copy)]
pub struct OffsetOnly;

impl ZoneSource for OffsetOnly {
    fn label(&self) -> String {
        "offset-only".into()
    }
    fn tzif(&self, name: &str) -> Result<Vec<u8>, TzError> {
        Err(TzError(format!(
            "time zone `{name}` is an IANA name, but the time zone source is offset-only; \
             use UTC or a fixed offset like +07:00, or set cron.tz_source = \"system\""
        )))
    }
}

/// A zoneinfo directory read at run time (no embedded data; the rules
/// follow the host's tz database updates).
#[derive(Debug, Clone)]
pub struct ZoneinfoDir {
    root: PathBuf,
}

impl ZoneinfoDir {
    pub fn new(root: PathBuf) -> Self {
        ZoneinfoDir { root }
    }
    /// `/usr/share/zoneinfo`.
    pub fn system() -> Self {
        Self::new(PathBuf::from("/usr/share/zoneinfo"))
    }
}

impl ZoneSource for ZoneinfoDir {
    fn label(&self) -> String {
        format!("system ({})", self.root.display())
    }

    fn tzif(&self, name: &str) -> Result<Vec<u8>, TzError> {
        let missing = |why: &str| {
            TzError(format!(
                "time zone `{name}` {why} in {}; check the name (an IANA name like \
                 Asia/Jakarta), install the tz database (tzdata), or use a fixed offset like +07:00",
                self.root.display()
            ))
        };
        let root = self
            .root
            .canonicalize()
            .map_err(|_| missing("cannot be looked up: no zoneinfo directory"))?;
        let path = root
            .join(name)
            .canonicalize()
            .map_err(|_| missing("is not found"))?;
        // Aliases are links inside the directory; nothing leads out of it.
        if !path.starts_with(&root) {
            return Err(missing("leads outside the zoneinfo directory"));
        }
        let mut opts = std::fs::OpenOptions::new();
        opts.read(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.custom_flags(libc::O_NONBLOCK);
        }
        let f = opts.open(&path).map_err(|_| missing("cannot be read"))?;
        let meta = f.metadata().map_err(|_| missing("cannot be read"))?;
        if !meta.is_file() || meta.len() > MAX_TZIF_BYTES {
            return Err(missing("is not a zone file"));
        }
        let mut buf = Vec::new();
        f.take(MAX_TZIF_BYTES + 1)
            .read_to_end(&mut buf)
            .map_err(|_| missing("cannot be read"))?;
        if buf.len() as u64 > MAX_TZIF_BYTES {
            return Err(missing("is not a zone file"));
        }
        Ok(buf)
    }
}

/// True for an IANA-style name: 1-64 bytes, `/`-separated components of
/// `[A-Za-z0-9_+-]`, none empty or starting with `.`.
pub fn valid_name(name: &str) -> bool {
    (1..=64).contains(&name.len())
        && name.split('/').all(|c| {
            !c.is_empty()
                && !c.starts_with('.')
                && c.bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"_+-".contains(&b))
        })
}

/// Resolves a job's `timezone`: `UTC`, a fixed offset `+HH:MM` / `-HH:MM`
/// (at most 14 hours), or an IANA name through `source`.
pub fn resolve(source: &dyn ZoneSource, name: &str) -> Result<Zone, TzError> {
    if name == "UTC" {
        return Ok(Zone::utc());
    }
    if name.starts_with(['+', '-']) {
        return parse_fixed(name).map(|secs| Zone {
            name: name.into(),
            rules: Rules::Fixed(secs),
        });
    }
    if !valid_name(name) {
        return Err(TzError(format!(
            "`{name}` is not a valid zone name; use UTC, a fixed offset like +07:00 or an \
             IANA name like Asia/Jakarta"
        )));
    }
    let bytes = source.tzif(name)?;
    Zone::from_tzif(name, &bytes)
}

fn parse_fixed(s: &str) -> Result<i32, TzError> {
    let b = s.as_bytes();
    let digits = |r: std::ops::Range<usize>| {
        b.get(r.clone())
            .filter(|d| d.iter().all(u8::is_ascii_digit))
            .map(|_| s[r].parse::<i32>().unwrap_or(-1))
    };
    let parsed = match (b.len(), b.get(3)) {
        (6, Some(b':')) => digits(1..3).zip(digits(4..6)),
        _ => None,
    };
    match parsed {
        Some((h, m)) if (0..=14).contains(&h) && (0..60).contains(&m) && h * 60 + m <= 14 * 60 => {
            let secs = h * 3_600 + m * 60;
            Ok(if b[0] == b'-' { -secs } else { secs })
        }
        _ => Err(TzError(format!(
            "`{s}` is not a valid UTC offset; write +HH:MM or -HH:MM, for example +07:00 \
             (at most 14 hours)"
        ))),
    }
}

/// Reads a big-endian integer of `N` bytes at `*pos`, advancing it.
fn take<const N: usize>(b: &[u8], pos: &mut usize) -> Option<[u8; N]> {
    let end = pos.checked_add(N)?;
    let out = b.get(*pos..end)?.try_into().ok()?;
    *pos = end;
    Some(out)
}

fn parse_tzif(b: &[u8]) -> Result<Table, String> {
    // Header: magic, version, 15 reserved bytes, six counts.
    let header = |pos: &mut usize| -> Result<(u8, [usize; 6]), String> {
        let magic: [u8; 4] = take(b, pos).ok_or("truncated header")?;
        if &magic != b"TZif" {
            return Err("not a TZif file".into());
        }
        let [version] = take::<1>(b, pos).ok_or("truncated header")?;
        take::<15>(b, pos).ok_or("truncated header")?;
        let mut counts = [0usize; 6];
        for c in counts.iter_mut() {
            *c = u32::from_be_bytes(take(b, pos).ok_or("truncated header")?) as usize;
        }
        Ok((version, counts))
    };
    let block = |c: &[usize; 6], time: usize, leap: usize| -> Option<usize> {
        let [isut, isstd, leapcnt, timecnt, typecnt, charcnt] = *c;
        timecnt
            .checked_mul(time + 1)?
            .checked_add(typecnt.checked_mul(6)?)?
            .checked_add(charcnt)?
            .checked_add(leapcnt.checked_mul(leap)?)?
            .checked_add(isstd)?
            .checked_add(isut)
    };
    let mut pos = 0;
    let (version, mut counts) = header(&mut pos)?;
    let mut time = 4;
    if version >= b'2' {
        let v1 = block(&counts, 4, 8).ok_or("counts overflow")?;
        pos = pos.checked_add(v1).ok_or("counts overflow")?;
        (_, counts) = header(&mut pos)?;
        time = 8;
    } else if version != 0 {
        return Err(format!("unknown version {version}"));
    }
    let [isut, isstd, leapcnt, timecnt, typecnt, _] = counts;
    if leapcnt != 0 {
        return Err("leap seconds (right/ zones) are not supported".into());
    }
    if typecnt == 0 || typecnt > 256 || timecnt > 100_000 {
        return Err("counts out of range".into());
    }
    if (isstd != 0 && isstd != typecnt) || (isut != 0 && isut != typecnt) {
        return Err("indicator counts do not match".into());
    }
    let size = block(&counts, time, 12).ok_or("counts overflow")?;
    if b.len().saturating_sub(pos) < size {
        return Err("truncated data".into());
    }
    // The footer follows the whole data block.
    let footer_at = pos + size;
    let mut at = Vec::with_capacity(timecnt);
    for _ in 0..timecnt {
        let t = if time == 8 {
            i64::from_be_bytes(take(b, &mut pos).ok_or("truncated data")?)
        } else {
            i64::from(i32::from_be_bytes(
                take(b, &mut pos).ok_or("truncated data")?,
            ))
        };
        if at.last().is_some_and(|last| *last >= t) {
            return Err("transitions out of order".into());
        }
        at.push(t);
    }
    let idx = b.get(pos..pos + timecnt).ok_or("truncated data")?.to_vec();
    pos += timecnt;
    let mut types = Vec::with_capacity(typecnt);
    for _ in 0..typecnt {
        let off = i32::from_be_bytes(take(b, &mut pos).ok_or("truncated data")?);
        take::<2>(b, &mut pos).ok_or("truncated data")?;
        if !(-MAX_OFFSET..=MAX_OFFSET).contains(&off) {
            return Err(format!("offset {off} s out of range"));
        }
        types.push(off);
    }
    let off = idx
        .iter()
        .map(|i| types.get(usize::from(*i)).copied())
        .collect::<Option<Vec<i32>>>()
        .ok_or("type index out of range")?;
    let tail = if version >= b'2' {
        let rest = b.get(footer_at..).unwrap_or_default();
        let footer = rest
            .strip_prefix(b"\n")
            .and_then(|r| r.iter().position(|c| *c == b'\n').map(|n| &r[..n]))
            .ok_or("missing footer")?;
        let text = std::str::from_utf8(footer).map_err(|_| "footer is not text")?;
        match text {
            "" => None,
            t => Some(Posix::parse(t).map_err(|e| format!("footer `{t}`: {e}"))?),
        }
    } else {
        None
    };
    Ok(Table {
        initial: types[0],
        at,
        off,
        tail,
    })
}

/// A POSIX TZ rule: standard offset and an optional DST period.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Posix {
    std: i32,
    dst: Option<Dst>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Dst {
    off: i32,
    /// Start date and local (standard) time of day in seconds.
    start: (Day, i32),
    /// End date and local (DST) time of day in seconds.
    end: (Day, i32),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Day {
    /// `Jn`: 1-365, February 29 never counted.
    Julian1(i64),
    /// `n`: 0-365, February 29 counted.
    Julian0(i64),
    /// `Mm.w.d`: weekday `d` of week `w` (5 = last) of month `m`.
    Month(u32, i64, u32),
}

impl Day {
    /// Days since 1970-01-01 of this rule day in year `y`.
    fn in_year(self, y: i64) -> i64 {
        let jan1 = days_from_civil(y, 1, 1);
        let leap = y % 4 == 0 && (y % 100 != 0 || y % 400 == 0);
        match self {
            Day::Julian1(n) => jan1 + n - 1 + i64::from(leap && n >= 60),
            Day::Julian0(n) => jan1 + n,
            Day::Month(m, w, d) => {
                let first = days_from_civil(y, m, 1);
                let (ny, nm) = if m == 12 { (y + 1, 1) } else { (y, m + 1) };
                let last = days_from_civil(ny, nm, 1) - 1;
                let mut day = first + i64::from((d + 7 - weekday(first)) % 7) + 7 * (w - 1);
                while day > last {
                    day -= 7;
                }
                day
            }
        }
    }
}

impl Posix {
    fn offset_at(&self, t: i64) -> i32 {
        let Some(d) = &self.dst else { return self.std };
        let (y, _, _) = civil_from_days(t.saturating_add(i64::from(self.std)).div_euclid(DAY));
        let instant = |(day, secs): (Day, i32), off: i32| {
            day.in_year(y)
                .saturating_mul(DAY)
                .saturating_add(i64::from(secs) - i64::from(off))
        };
        let (start, end) = (instant(d.start, self.std), instant(d.end, d.off));
        let in_dst = if start < end {
            t >= start && t < end
        } else {
            !(t >= end && t < start)
        };
        if in_dst {
            d.off
        } else {
            self.std
        }
    }

    fn parse(s: &str) -> Result<Posix, String> {
        let mut p = Cursor {
            b: s.as_bytes(),
            i: 0,
        };
        p.name()?;
        let std = -p.time(24)?;
        if p.done() {
            return Ok(Posix { std, dst: None });
        }
        p.name()?;
        let off = if p
            .peek()
            .is_some_and(|c| c.is_ascii_digit() || c == b'+' || c == b'-')
        {
            -p.time(24)?
        } else {
            std + 3_600
        };
        let (start, end) = if p.eat(b',') {
            let start = p.rule()?;
            if !p.eat(b',') {
                return Err("expected `,` before the end rule".into());
            }
            (start, p.rule()?)
        } else {
            // POSIX leaves the default open; the US rule is the common one.
            ((Day::Month(3, 2, 0), 7_200), (Day::Month(11, 1, 0), 7_200))
        };
        if !p.done() {
            return Err(format!("unexpected text at byte {}", p.i));
        }
        if !(-MAX_OFFSET..=MAX_OFFSET).contains(&std) || !(-MAX_OFFSET..=MAX_OFFSET).contains(&off)
        {
            return Err("offset out of range".into());
        }
        Ok(Posix {
            std,
            dst: Some(Dst { off, start, end }),
        })
    }
}

struct Cursor<'a> {
    b: &'a [u8],
    i: usize,
}

impl Cursor<'_> {
    fn peek(&self) -> Option<u8> {
        self.b.get(self.i).copied()
    }
    fn done(&self) -> bool {
        self.i >= self.b.len()
    }
    fn eat(&mut self, c: u8) -> bool {
        let hit = self.peek() == Some(c);
        self.i += usize::from(hit);
        hit
    }
    /// `<...>` or at least three letters.
    fn name(&mut self) -> Result<(), String> {
        let start = self.i;
        if self.eat(b'<') {
            while self
                .peek()
                .is_some_and(|c| c.is_ascii_alphanumeric() || c == b'+' || c == b'-')
            {
                self.i += 1;
            }
            if self.i - start < 4 || !self.eat(b'>') {
                return Err("bad quoted zone abbreviation".into());
            }
        } else {
            while self.peek().is_some_and(|c| c.is_ascii_alphabetic()) {
                self.i += 1;
            }
            if self.i - start < 3 {
                return Err("zone abbreviation needs at least 3 letters".into());
            }
        }
        Ok(())
    }
    fn number(&mut self, max: i64) -> Result<i64, String> {
        let start = self.i;
        while self.peek().is_some_and(|c| c.is_ascii_digit()) && self.i - start < 4 {
            self.i += 1;
        }
        let n: i64 = std::str::from_utf8(&self.b[start..self.i])
            .ok()
            .and_then(|t| t.parse().ok())
            .ok_or("expected a number")?;
        if n > max {
            return Err(format!("{n} is out of range (max {max})"));
        }
        Ok(n)
    }
    /// `[+-]h[:mm[:ss]]` in seconds, hours at most `max_h`.
    fn time(&mut self, max_h: i64) -> Result<i32, String> {
        let neg = self.eat(b'-');
        if !neg {
            self.eat(b'+');
        }
        let mut secs = self.number(max_h)? * 3_600;
        if self.eat(b':') {
            secs += self.number(59)? * 60;
            if self.eat(b':') {
                secs += self.number(59)?;
            }
        }
        Ok((if neg { -secs } else { secs }) as i32)
    }
    /// `Jn`, `n` or `Mm.w.d`, then an optional `/time` (default 02:00).
    fn rule(&mut self) -> Result<(Day, i32), String> {
        let day = if self.eat(b'J') {
            let n = self.number(365)?;
            if n < 1 {
                return Err("Jn day must be 1-365".into());
            }
            Day::Julian1(n)
        } else if self.eat(b'M') {
            let m = self.number(12)?;
            let dot = |c: &mut Self| c.eat(b'.').then_some(()).ok_or("expected `.`");
            dot(self)?;
            let w = self.number(5)?;
            dot(self)?;
            let d = self.number(6)?;
            if m < 1 || w < 1 {
                return Err("Mm.w.d needs month 1-12 and week 1-5".into());
            }
            Day::Month(m as u32, w, d as u32)
        } else {
            Day::Julian0(self.number(365)?)
        };
        let time = if self.eat(b'/') {
            self.time(167)?
        } else {
            7_200
        };
        Ok((day, time))
    }
}
