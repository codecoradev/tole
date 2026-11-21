//! One row of the Automations table.

use super::run::RunOutcome;
use super::schedule::Schedule;
use crate::plain::printable;

/// The id of a job (never its name: names repeat, ids do not). It is the exact
/// string the server gave and is never changed: it is what pause and resume send
/// back, so a cut or cleaned copy could name another job (`tole cron create --id`
/// takes any text). To draw it use [`AutomationId::display`], never `as_str`.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct AutomationId(String);

impl AutomationId {
    pub fn new(id: &str) -> AutomationId {
        AutomationId(id.to_owned())
    }

    /// The exact id, for the request (the adapter percent-encodes it).
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The id made safe to draw.
    pub fn display(&self) -> String {
        printable(&self.0, 80)
    }
}

/// Who saved the job. `cli` is its owner; `agent` is a job Tole saved with its
/// `cron_create` tool; any other word is `Unknown` and is treated as not the
/// owner's.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Origin {
    Owner,
    Tole,
    Unknown,
}

/// The on/off switch. It reads `On` only for a job that is active and that the
/// owner stands behind; anything else is `Off`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Switch {
    On,
    Off,
}

/// The "Next" column.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NextRun {
    Paused,
    /// The server's own text for the next run (in the job's zone and UTC).
    At(String),
    /// Active but the scheduler names no next run.
    NotScheduled,
}

/// The small line under a name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RowNote {
    /// Saved by Tole and not reviewed: paused until the owner turns it on.
    MadeByTolePaused,
    /// Not saved by the owner and not reviewed, by someone or something this
    /// model does not know.
    NotYoursPaused,
    /// Saved by Tole, and the owner turned it on.
    MadeByTole,
}

/// A job of `GET /cron/jobs`, as the adapter hands it over (mapping of the
/// client's `CronJob`, field for field). `folder` is the job's working folder:
/// the serve row does not carry it today, so the adapter passes `None` until
/// the route does.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RawJob {
    pub id: String,
    pub name: String,
    pub schedule: String,
    pub timezone: String,
    pub state: String,
    pub next_run: Option<i64>,
    pub next: String,
    pub last: Option<String>,
    pub missed: u64,
    pub blocked: u64,
    pub created_by: String,
    pub awaiting_review: bool,
    pub folder: Option<String>,
}

/// One automation, ready to draw.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AutomationRow {
    pub id: AutomationId,
    pub name: String,
    pub schedule: Schedule,
    /// The zone the schedule runs in (`UTC` when the job has none).
    pub timezone: String,
    pub folder: Option<String>,
    pub next: NextRun,
    /// How the last run ended; `None` when it never ran.
    pub last: Option<RunOutcome>,
    pub switch: Switch,
    pub origin: Origin,
    /// An agent's job the owner has not enabled: serve refuses to resume it.
    pub awaiting_review: bool,
    /// Runs skipped while the scheduler was down.
    pub missed: u64,
    /// Runs the safety check refused to start.
    pub blocked: u64,
}

impl AutomationRow {
    pub fn new(raw: RawJob) -> AutomationRow {
        let origin = match raw.created_by.as_str() {
            "cli" => Origin::Owner,
            "agent" => Origin::Tole,
            _ => Origin::Unknown,
        };
        let on = raw.state == "active" && !raw.awaiting_review;
        let next = if !on {
            NextRun::Paused
        } else if raw.next_run.is_some() {
            NextRun::At(printable(&raw.next, 80))
        } else {
            NextRun::NotScheduled
        };
        AutomationRow {
            id: AutomationId::new(&raw.id),
            name: printable(&raw.name, 80),
            schedule: Schedule::describe(&raw.schedule),
            timezone: printable(&raw.timezone, 64),
            folder: raw
                .folder
                .map(|f| printable(&f, 120))
                .filter(|f| !f.is_empty()),
            next,
            last: raw
                .last
                .as_deref()
                .map(|s| RunOutcome::from_status(Some(s))),
            switch: if on { Switch::On } else { Switch::Off },
            origin,
            awaiting_review: raw.awaiting_review,
            missed: raw.missed,
            blocked: raw.blocked,
        }
    }

    /// Whether the owner can turn it on from the window: not while it awaits
    /// their review at a terminal.
    pub fn can_resume_here(&self) -> bool {
        !self.awaiting_review
    }

    pub fn note(&self) -> Option<RowNote> {
        match (self.origin, self.awaiting_review) {
            (Origin::Owner, _) => None,
            (Origin::Tole, true) => Some(RowNote::MadeByTolePaused),
            (Origin::Unknown, true) => Some(RowNote::NotYoursPaused),
            (Origin::Tole, false) => Some(RowNote::MadeByTole),
            (Origin::Unknown, false) => None,
        }
    }
}

#[cfg(test)]
pub(crate) fn raw_job(id: &str, created_by: &str, state: &str, review: bool) -> RawJob {
    RawJob {
        id: id.into(),
        name: format!("job {id}"),
        schedule: "0 2 * * *".into(),
        timezone: "UTC".into(),
        state: state.into(),
        next_run: (state == "active").then_some(1_000),
        next: "2026-11-22 02:00 UTC".into(),
        last: None,
        missed: 0,
        blocked: 0,
        created_by: created_by.into(),
        awaiting_review: review,
        folder: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_id_is_kept_exactly_for_the_request_and_cleaned_only_to_be_drawn() {
        let raw = format!("night\u{1b}[31m\n{}", "x".repeat(100));
        let id = AutomationId::new(&raw);
        assert_eq!(id.as_str(), raw, "what is sent back is never changed");
        let shown = id.display();
        assert!(shown.chars().count() <= 80 && !shown.contains('\u{1b}') && !shown.contains('\n'));
        assert_ne!(
            AutomationId::new("a"),
            AutomationId::new("a\u{200B}"),
            "two ids stay two ids"
        );
    }

    #[test]
    fn a_job_tole_made_starts_paused_and_says_so() {
        let row = AutomationRow::new(raw_job("a", "agent", "paused", true));
        assert_eq!(row.switch, Switch::Off);
        assert_eq!(row.next, NextRun::Paused);
        assert_eq!(row.note(), Some(RowNote::MadeByTolePaused));
        assert!(!row.can_resume_here());
        let on = AutomationRow::new(raw_job("a", "agent", "active", false));
        assert_eq!(
            (on.switch, on.note()),
            (Switch::On, Some(RowNote::MadeByTole))
        );
    }

    /// A job nobody reviewed never reads as "on", even if a server said
    /// `active`, and an unknown origin is never the owner's.
    #[test]
    fn the_switch_is_on_only_for_an_active_job_the_owner_stands_behind() {
        let odd = AutomationRow::new(raw_job("a", "agent", "active", true));
        assert_eq!((odd.switch, odd.next), (Switch::Off, NextRun::Paused));
        for state in ["", "running", "ACTIVE"] {
            assert_eq!(
                AutomationRow::new(raw_job("a", "cli", state, false)).switch,
                Switch::Off
            );
        }
        let other = AutomationRow::new(raw_job("a", "plugin", "paused", true));
        assert_eq!(
            (other.origin, other.note()),
            (Origin::Unknown, Some(RowNote::NotYoursPaused))
        );
        assert_eq!(
            AutomationRow::new(raw_job("a", "cli", "active", false)).note(),
            None
        );
    }

    #[test]
    fn last_result_and_next_run_read_from_the_job() {
        let mut raw = raw_job("a", "cli", "active", false);
        assert_eq!(AutomationRow::new(raw.clone()).last, None);
        raw.last = Some("awaiting_approval".into());
        raw.next_run = None;
        let row = AutomationRow::new(raw);
        assert_eq!(row.last, Some(RunOutcome::StoppedToAsk));
        assert_eq!(row.next, NextRun::NotScheduled);
    }
}
