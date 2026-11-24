//! Words of the Automations page (board S6). Wording is a draft, as in the
//! rest of [`crate::strings`].

use crate::automations::{
    AutomationRow, NextRun, Notice, RowNote, RunOutcome, Schedule, Scheduler, SchedulerHost,
    OWNER_RESUME_COMMAND,
};

pub const TITLE: &str = "Automations";
pub const SUBTITLE: &str =
    "Tasks Tole does on a schedule. They stop to ask you when they need an OK.";
pub const COLUMN_NAMES: [&str; 6] = ["Name", "When", "Folder", "Next", "Last result", "On"];
pub const EMPTY: &str = "No automations yet";
pub const NO_FOLDER: &str = "Not shown";
pub const NEVER_RAN: &str = "Never ran";
pub const HISTORY_EMPTY: &str = "Nothing has run yet";
pub const OPEN_IN_APPROVALS: &str = "Open in Approvals";
pub const LOAD_FAILED: &str = "Tole could not read the automations";
pub const DISMISS: &str = "Dismiss";
pub const CHECKING: &str = "Checking the automations...";
pub const OUT_OF_DATE: &str = "This list may be out of date. Tole could not read it again.";
pub const HISTORY_LOADING: &str = "Reading what happened...";
pub const HISTORY_FAILED: &str = "Tole could not read what happened";
pub const LINKS_UNKNOWN: &str =
    "Tole could not tell which runs are waiting in Approvals. Open Approvals to see them.";

/// "3 more not shown" (jobs the table has no room for).
pub fn more_not_shown(count: usize) -> String {
    format!("{count} more not shown")
}

/// The title of the history of one automation.
pub fn history_title(name: &str) -> String {
    format!("{name}: what happened")
}

/// "At least 5 older runs are not shown".
pub fn history_more(count: usize) -> String {
    format!("At least {count} older runs are not shown")
}

/// "2 on, 1 paused" (the top bar).
pub fn summary(on: usize, paused: usize) -> String {
    format!("{on} on, {paused} paused")
}

/// The "When" column: the sentence, or the expression itself when it has no
/// sentence. The zone follows, because "02:00" means nothing without it.
pub fn schedule_line(schedule: &Schedule, timezone: &str) -> String {
    match schedule.words() {
        Some(words) => format!("{words} ({timezone})"),
        None => format!("Custom schedule: {}", schedule.expr()),
    }
}

/// The "Next" column.
pub fn next_label(next: &NextRun) -> String {
    match next {
        NextRun::Paused => "Paused".to_owned(),
        NextRun::At(text) => text.clone(),
        NextRun::NotScheduled => "Not scheduled".to_owned(),
    }
}

/// How a run ended; `None` is a job that never ran.
pub fn outcome_label(outcome: Option<&RunOutcome>) -> String {
    match outcome {
        None => NEVER_RAN.to_owned(),
        Some(RunOutcome::Passed) => "Passed".to_owned(),
        Some(RunOutcome::Failed) => "Failed".to_owned(),
        Some(RunOutcome::StoppedToAsk) => "Stopped to ask you".to_owned(),
        Some(RunOutcome::StoppedByLimit) => "Stopped: spending limit reached".to_owned(),
        Some(RunOutcome::Cancelled) => "Cancelled".to_owned(),
        Some(RunOutcome::Interrupted) => "Interrupted when Tole stopped".to_owned(),
        Some(RunOutcome::NoResultYet) => "No result yet".to_owned(),
        Some(RunOutcome::Unknown(word)) => format!("Unknown result ({word})"),
    }
}

/// The small line under a name.
pub fn row_note(note: RowNote) -> &'static str {
    match note {
        RowNote::MadeByTolePaused => "Made by Tole. Paused until you turn it on.",
        RowNote::NotYoursPaused => "Not saved by you. Paused until you review it.",
        RowNote::MadeByTole => "Made by Tole. You turned it on.",
    }
}

/// A line of the row's problems, when it has any.
pub fn row_warning(row: &AutomationRow) -> Option<String> {
    match (row.missed, row.blocked) {
        (0, 0) => None,
        (m, 0) => Some(format!("{m} run(s) were missed while Tole was not running")),
        (0, b) => Some(format!("{b} run(s) were refused by the safety check")),
        (m, b) => Some(format!(
            "{m} run(s) were missed and {b} refused by the safety check"
        )),
    }
}

/// What the page says about where automations run. `remote` is a serve on
/// another computer.
pub fn scheduler_note(scheduler: &Scheduler, remote: bool) -> String {
    let place = if remote {
        "on that computer"
    } else {
        "on this computer"
    };
    match scheduler {
        Scheduler::NotRunning => format!(
            "No scheduler is running {place}, so automations do not run. \
             Start Tole with its scheduler (`tole serve --cron`)."
        ),
        Scheduler::Running(SchedulerHost::Serve) if remote => {
            "Automations run on that computer while Tole runs there. Closing this window changes nothing.".to_owned()
        }
        Scheduler::Running(SchedulerHost::Serve) => {
            "Automations run while Tole runs in the background. Closing the window does not stop them; Quit Tole completely does.".to_owned()
        }
        Scheduler::Running(_) => format!("A separate scheduler runs the automations {place}."),
    }
}

/// The message under the table after a switch.
pub fn notice_text(notice: &Notice) -> String {
    match notice {
        Notice::NeedsOwnerReview { name } => format!(
            "\u{201C}{name}\u{201D} was not saved by you and you have not reviewed it. \
             To turn it on, review it and run `{OWNER_RESUME_COMMAND}` in a terminal."
        ),
        Notice::ServerPlanMode => {
            "This Tole runs in plan mode: it can pause automations but not turn them on.".to_owned()
        }
        Notice::Gone => "That automation no longer exists.".to_owned(),
        Notice::NotReached => "Tole did not answer. Nothing was changed.".to_owned(),
        Notice::Failed(why) => format!("That did not work: {why}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_schedule_without_a_sentence_shows_its_expression() {
        let known = Schedule::describe("0 2 * * *");
        assert_eq!(schedule_line(&known, "UTC"), "Every night at 02:00 (UTC)");
        let odd = Schedule::describe("0 0 13 * 5");
        assert_eq!(schedule_line(&odd, "UTC"), "Custom schedule: 0 0 13 * 5");
    }

    #[test]
    fn the_review_notice_names_the_owner_action() {
        let text = notice_text(&Notice::NeedsOwnerReview {
            name: "Check invoices".into(),
        });
        assert!(text.contains("`tole cron resume`") && text.contains("in a terminal"));
        assert!(text.contains("Check invoices"));
    }

    #[test]
    fn no_scheduler_never_reads_as_a_kept_promise() {
        let note = scheduler_note(&Scheduler::NotRunning, false);
        assert!(note.contains("do not run") && note.contains("--cron"));
        let running = scheduler_note(&Scheduler::Running(SchedulerHost::Serve), false);
        assert!(running.contains("Closing the window does not stop"));
        assert!(
            scheduler_note(&Scheduler::Running(SchedulerHost::Serve), true)
                .contains("that computer")
        );
    }

    #[test]
    fn an_unknown_result_is_named_and_not_called_a_pass() {
        let label = outcome_label(Some(&RunOutcome::Unknown("weird".into())));
        assert!(label.contains("weird") && !label.contains("Passed"));
        assert_eq!(outcome_label(None), NEVER_RAN);
        assert_eq!(summary(2, 1), "2 on, 1 paused");
    }
}
