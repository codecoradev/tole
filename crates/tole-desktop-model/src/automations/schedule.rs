//! A cron expression in words ("Every night at 02:00").
//!
//! Pure and deliberately small: it understands the shapes people write most
//! and gives `None` for everything else, so the page shows the raw expression
//! instead. A wrong sentence would be worse than none, so every rule below
//! follows the scheduler's own grammar (`tole-core` `ledger/cron.rs`: a day
//! field that starts with `*` is unrestricted, and when both day fields are
//! restricted a day matches if EITHER does, which has no short sentence).

use crate::plain::printable;

/// A schedule as the page shows it: the words when the expression is a shape
/// this formatter knows, always the expression itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Schedule {
    expr: String,
    words: Option<String>,
}

impl Schedule {
    /// From the expression of a job (`schedule` of `GET /cron/jobs`): a cron
    /// expression, a macro (`@daily`) or `@every 1h30m`.
    pub fn describe(expr: &str) -> Schedule {
        Schedule {
            expr: printable(expr, 256),
            words: words(expr),
        }
    }

    /// The sentence, or `None` when the expression is not a known shape.
    pub fn words(&self) -> Option<&str> {
        self.words.as_deref()
    }

    /// The expression as the job stores it (safe to draw).
    pub fn expr(&self) -> &str {
        &self.expr
    }
}

fn words(expr: &str) -> Option<String> {
    let lower = expr.trim().to_ascii_lowercase();
    if let Some(rest) = lower
        .strip_prefix("@every ")
        .or_else(|| lower.strip_prefix("every "))
    {
        return every(rest.trim());
    }
    let body = match lower.as_str() {
        "@hourly" => "0 * * * *",
        "@daily" | "@midnight" => "0 0 * * *",
        "@weekly" => "0 0 * * 0",
        "@monthly" => "0 0 1 * *",
        other => other,
    };
    let fields: Vec<&str> = body.split_whitespace().collect();
    let [min, hour, dom, month, dow] = fields[..] else {
        return None;
    };
    if month != "*" {
        return None;
    }
    if hour == "*" && dom == "*" && dow == "*" {
        if let Some(step) = min.strip_prefix("*/") {
            let n: u32 = digits(step)?;
            // Only a step that divides the hour is a steady rhythm; `*/7`
            // restarts at the top of every hour.
            return (n >= 1 && 60 % n == 0).then(|| match n {
                1 => "Every minute".to_owned(),
                n => format!("Every {n} minutes"),
            });
        }
        if min == "*" {
            return Some("Every minute".to_owned());
        }
    }
    let m = digits(min).filter(|m| *m <= 59)?;
    if hour == "*" {
        return (dom == "*" && dow == "*").then(|| match m {
            0 => "Every hour, on the hour".to_owned(),
            m => format!("Every hour at {m} minutes past"),
        });
    }
    let h = digits(hour).filter(|h| *h <= 23)?;
    let time = format!("{h:02}:{m:02}");
    match (dom, dow) {
        ("*", "*") if h <= 4 => Some(format!("Every night at {time}")),
        ("*", "*") => Some(format!("Every day at {time}")),
        ("*", days) => Some(format!("Every {} at {time}", day_names(days)?)),
        // Past the 28th some months have no such day: no sentence for those.
        (day, "*") => digits(day)
            .filter(|d| (1..=28).contains(d))
            .map(|d| format!("On day {d} of every month at {time}")),
        _ => None,
    }
}

/// A plain run of one or two ASCII digits (the scheduler's own numbers).
fn digits(s: &str) -> Option<u32> {
    if s.is_empty() || s.len() > 2 || !s.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    s.parse().ok()
}

const DAYS: [&str; 7] = [
    "Sunday",
    "Monday",
    "Tuesday",
    "Wednesday",
    "Thursday",
    "Friday",
    "Saturday",
];

/// One day-of-week value: 0 to 7 (0 and 7 are Sunday) or `sun` to `sat`.
fn dow_value(s: &str) -> Option<usize> {
    let short = ["sun", "mon", "tue", "wed", "thu", "fri", "sat"];
    short
        .iter()
        .position(|n| *n == s)
        .or_else(|| digits(s).filter(|d| *d <= 7).map(|d| d as usize))
}

/// "weekday", "Monday", "Monday and Thursday"... for a list of days and
/// ranges; `None` for steps and anything else.
fn day_names(field: &str) -> Option<String> {
    let mut set = [false; 7];
    for item in field.split(',') {
        let (lo, hi) = match item.split_once('-') {
            Some((a, b)) => (dow_value(a)?, dow_value(b)?),
            None => (dow_value(item)?, dow_value(item)?),
        };
        if lo > hi {
            return None;
        }
        for d in lo..=hi {
            set[d % 7] = true;
        }
    }
    if set.iter().all(|d| *d) {
        return Some("day".to_owned());
    }
    if set == [false, true, true, true, true, true, false] {
        return Some("weekday".to_owned());
    }
    if set == [true, false, false, false, false, false, true] {
        return Some("weekend".to_owned());
    }
    // Monday first, Sunday last.
    let names: Vec<&str> = (1..=7)
        .filter(|d| set[d % 7])
        .map(|d| DAYS[d % 7])
        .collect();
    Some(match names.split_last() {
        Some((last, [])) => (*last).to_owned(),
        Some((last, head)) => format!("{} and {last}", head.join(", ")),
        None => return None,
    })
}

/// `1h30m`, `2h`, `45m`: units `d h m s`, each once, at least 60 seconds in
/// all (the scheduler refuses less).
fn every(text: &str) -> Option<String> {
    let units = [
        ('d', "day"),
        ('h', "hour"),
        ('m', "minute"),
        ('s', "second"),
    ];
    let (mut parts, mut seen, mut total, mut n) = (Vec::new(), Vec::new(), 0u64, String::new());
    for c in text.chars() {
        if c.is_ascii_digit() {
            n.push(c);
            continue;
        }
        let (idx, (_, name)) = units.iter().enumerate().find(|(_, (u, _))| *u == c)?;
        let count: u64 = n.parse().ok().filter(|v: &u64| *v < 100_000)?;
        n.clear();
        if seen.contains(&idx) {
            return None;
        }
        seen.push(idx);
        total += count * [86_400, 3_600, 60, 1][idx];
        if count > 0 {
            parts.push(format!(
                "{count} {name}{}",
                if count == 1 { "" } else { "s" }
            ));
        }
    }
    (n.is_empty() && total >= 60 && !parts.is_empty()).then(|| format!("Every {}", parts.join(" ")))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn w(expr: &str) -> Option<String> {
        Schedule::describe(expr).words().map(str::to_owned)
    }

    #[test]
    fn the_common_shapes_read_as_sentences() {
        for (expr, want) in [
            ("0 2 * * *", "Every night at 02:00"),
            ("30 9 * * *", "Every day at 09:30"),
            ("@daily", "Every night at 00:00"),
            ("30 8 * * 1-5", "Every weekday at 08:30"),
            ("0 10 * * 6,0", "Every weekend at 10:00"),
            ("0 17 * * fri", "Every Friday at 17:00"),
            (
                "0 9 * * 1,3,5",
                "Every Monday, Wednesday and Friday at 09:00",
            ),
            ("0 9 * * 7", "Every Sunday at 09:00"),
            ("0 9 * * 0-6", "Every day at 09:00"),
            ("0 9 1 * *", "On day 1 of every month at 09:00"),
            ("@monthly", "On day 1 of every month at 00:00"),
            ("@hourly", "Every hour, on the hour"),
            ("15 * * * *", "Every hour at 15 minutes past"),
            ("*/15 * * * *", "Every 15 minutes"),
            ("* * * * *", "Every minute"),
            ("@every 1h30m", "Every 1 hour 30 minutes"),
            ("every 2h", "Every 2 hours"),
        ] {
            assert_eq!(w(expr).as_deref(), Some(want), "{expr}");
        }
    }

    /// Anything the formatter is not sure of shows the raw expression, never
    /// a sentence that could be wrong.
    #[test]
    fn what_it_does_not_know_has_no_sentence() {
        for expr in [
            "",
            "0 2 * *",
            "0 2 * * * *",
            "*/7 * * * *",
            "0 */2 * * *",
            "0 9,17 * * *",
            "0 9 * 6 *",
            "0 0 13 * 5",
            "0 9 31 * *",
            "0 9 */2 * *",
            "0 9 * * */2",
            "0 9 * * 5-1",
            "0 9 * * 8",
            "60 9 * * *",
            "0 24 * * *",
            "@yearly",
            "@every 30s",
            "@every 1h1h",
            "@every 5",
            "@every 0m",
            "not a schedule",
        ] {
            assert_eq!(w(expr), None, "{expr:?}");
            assert_eq!(Schedule::describe(expr).expr(), expr.trim());
        }
    }

    #[test]
    fn the_expression_is_safe_to_draw() {
        assert_eq!(
            Schedule::describe("0 2 * * *\u{1b}[0m").expr(),
            "0 2 * * * [0m"
        );
    }
}
