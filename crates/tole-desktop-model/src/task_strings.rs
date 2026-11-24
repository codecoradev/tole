//! Words of the task view (board S3). Reached as `strings::task`.
//!
//! Plain words, still a draft like the rest of the table. The rules that pick
//! a sentence live in `crate::task`; these functions turn a state into words.

use crate::task::{
    AttachError, Counts, Mark, Phase, PlanStatus, SendMode, StatusRow, Step, StepState, ToolEffect,
    MAX_IMAGES, MAX_IMAGE_BYTES,
};

fn plural(n: u32, one: &str, many: &str) -> String {
    format!("{n} {}", if n == 1 { one } else { many })
}

/// The status sentence: "Working. Nothing has been changed yet."
pub fn status_text(row: &StatusRow) -> String {
    let done = row.phase == Phase::Idle;
    let head = match row.phase {
        Phase::Idle if !row.started => return "Ready.".to_string(),
        Phase::Idle => "Done.",
        Phase::Working => "Working.",
        Phase::Resuming => "Continuing after your OK.",
        Phase::WaitingApproval => "Waiting for your OK.",
        Phase::Stopping => "Stopping.",
        Phase::Offline => {
            return "Lost the connection to Tole. Reopen the task to try again.".into()
        }
        Phase::Closed => return "This task has ended.".to_string(),
    };
    let (n, unsure) = (row.counts.changed, row.counts.unsure);
    let made = plural(n, "change", "changes");
    let changes = match (n, unsure, done) {
        (0, 0, false) if !row.complete => "No changes seen yet.".to_string(),
        (0, 0, true) if !row.complete => "No changes seen.".to_string(),
        (0, 0, false) => "Nothing has been changed yet.".to_string(),
        (0, 0, true) => "Nothing was changed.".to_string(),
        (0, _, _) => "Something may have been changed.".to_string(),
        (1, 0, true) => "1 change was made.".to_string(),
        (_, 0, true) => format!("{n} changes were made."),
        (_, 0, false) => format!("{made} made so far."),
        (_, _, true) => format!("{made} made. Something else may have been changed."),
        (_, _, false) => format!("{made} made so far. Something else may have been changed."),
    };
    format!("{head} {changes}")
}

/// The "Look only" chip: how the task was started.
pub fn look_only_chip(on: bool) -> &'static str {
    ["Look only: off", "Look only: on"][usize::from(on)]
}

pub const STOP_LABEL: &str = "Stop";

/// "38 read · 0 changed".
pub fn counts_label(counts: Counts) -> String {
    format!("{} read · {} changed", counts.read, counts.changed)
}

/// "Working · 3 steps" while going on, "Worked · 3 steps" after.
pub fn group_header(steps: u32, active: bool) -> String {
    let lead = if active { "Working" } else { "Worked" };
    format!("{lead} · {}", plural(steps, "step", "steps"))
}

/// One step in plain words; the tool and target are "technical details".
pub fn step_label(step: &Step) -> String {
    let target = step.target.as_deref().filter(|t| !t.is_empty());
    match (step.tool.as_str(), target) {
        ("read_file" | "read_file_raw", Some(t)) => format!("Read {t}"),
        ("web_fetch", Some(t)) => format!("Opened {t}"),
        ("write_file", Some(t)) => format!("Wrote {t}"),
        ("edit_file", Some(t)) => format!("Edited {t}"),
        ("delete_file", Some(t)) => format!("Deleted {t}"),
        ("run_command", _) => "Ran a program".to_string(),
        _ if step.effect == ToolEffect::Read => "Looked something up".to_string(),
        (tool, _) => format!("Used {tool}"),
    }
}

/// The composer's hint for a mode.
pub fn composer_placeholder(mode: SendMode) -> &'static str {
    match mode {
        SendMode::Send => "Add a note or ask Tole something...",
        SendMode::Steer => "Tell Tole what to do differently...",
        SendMode::Disabled(Phase::WaitingApproval) => {
            "Tole is waiting for your OK before it goes on."
        }
        SendMode::Disabled(Phase::Stopping) => "Stopping. You can write again in a moment.",
        SendMode::Disabled(Phase::Resuming) => "Tole is picking up where it left off.",
        SendMode::Disabled(Phase::Closed) => "This task has ended.",
        SendMode::Disabled(_) => "Not connected.",
    }
}

/// Why an image was not added.
pub fn attach_error_text(error: AttachError) -> String {
    match error {
        AttachError::Empty => "That image is empty.".to_string(),
        AttachError::TooLarge { .. } => {
            let mb = MAX_IMAGE_BYTES / (1024 * 1024);
            format!("That image is larger than {mb} MB.")
        }
        AttachError::TooMany => format!("You can add up to {MAX_IMAGES} images to one message."),
        AttachError::NotAnImage => "Tole can read PNG, JPEG, GIF and WebP images.".to_string(),
    }
}

/// The mark before a list item of an answer.
pub const BULLET: &str = "\u{2022}";
/// The mark before an item of a numbered list: `3.`.
pub fn list_number(number: u32) -> String {
    format!("{number}.")
}

/// The marks of a "Working" group that is folded and one that is open.
pub const GROUP_FOLDED: &str = "\u{25B8}";
pub const GROUP_OPEN: &str = "\u{25BE}";

/// A step's tool and target, as the technical details show them.
pub fn technical_line(step: &Step) -> String {
    format!(
        "{} {}",
        step.tool,
        step.target.as_deref().unwrap_or_default()
    )
}

/// The link under an opened "Working" group (board S3).
pub const TECH_SHOW: &str = "Show technical details";
pub const TECH_HIDE: &str = "Hide technical details";

/// How a step stands, at the end of its row.
pub fn step_state(state: StepState) -> &'static str {
    match state {
        StepState::Running => "working",
        StepState::Done => "done",
        StepState::Failed => "did not work",
        StepState::Unfinished => "stopped",
    }
}

/// How a plan item stands: the mark before its text.
pub fn plan_mark(status: PlanStatus) -> &'static str {
    match status {
        PlanStatus::Pending => "To do",
        PlanStatus::InProgress => "Doing",
        PlanStatus::Completed => "Done",
    }
}

/// The caption above a plan list.
pub const PLAN_CAPTION: &str = "Plan";

/// The mark at the foot of a running transcript.
pub fn mark_label(mark: Mark) -> &'static str {
    match mark {
        Mark::Thinking => "Thinking...",
        Mark::Writing => "Writing...",
    }
}

/// The button that returns a list scrolled up to its end.
pub const JUMP_TO_LATEST: &str = "Jump to latest";

/// Above a note the owner sent to a running turn.
pub const STEER_LABEL: &str = "Your note to Tole";
/// Above Tole's own note to the model (a refused call, a stop gate).
pub const NOTE_LABEL: &str = "Note from Tole";
/// Under an answer that hit the length limit.
pub const ANSWER_CUT: &str = "This answer was very long, so only the first part is shown.";

/// Under a message that came with pictures.
pub fn images_label(count: u32) -> String {
    format!("{} attached", plural(count, "image", "images"))
}

/// Shown while the history of a task is being read.
pub const LOADING: &str = "Opening the task...";
/// Shown by a task that has no words yet.
pub const EMPTY_TASK: &str =
    "Tell Tole what you would like done. It shows its plan and asks before it changes anything.";
/// The tab of a task that has no first message yet.
pub const NEW_TASK_TAB: &str = "New task";
/// The tab button that closes a task.
pub const CLOSE_TAB: &str = "Close this task";
/// The tab button that starts another.
pub const ADD_TAB: &str = "New task";
/// What those two buttons show.
pub const CLOSE_MARK: &str = "\u{00D7}";
pub const ADD_MARK: &str = "+";
/// A fourth task was asked for while three are open.
pub fn too_many_tasks(max: usize) -> String {
    format!("{max} tasks are open. Close one to open another.")
}

/// The button that sends the composer's text.
pub fn send_label(mode: SendMode) -> &'static str {
    match mode {
        SendMode::Steer => "Send the note",
        _ => "Send",
    }
}
/// The composer while a text is not sendable yet: the keys it takes.
pub const COMPOSER_KEYS: &str = "Enter to send, Shift+Enter for a new line";
/// Under the status line while Tole waits for an OK: where to answer.
pub const OPEN_APPROVALS: &str = "Open Approvals";
/// The send was refused (the task is not connected): the words stay.
pub const SEND_REFUSED: &str = "Tole could not take that just now. Your words are still here.";
/// A send that may or may not have arrived (see `TaskView::last_send_uncertain`).
pub const SEND_UNCERTAIN: &str =
    "Tole may or may not have received your last message. Check the conversation before you send it again.";

#[cfg(test)]
mod tests {
    use super::*;
    use crate::task::{tool_effect, StepState};

    #[rustfmt::skip]
    fn row_for(phase: Phase) -> StatusRow {
        let counts = Counts { read: 0, changed: 0, unsure: 0 };
        StatusRow { phase, counts, steps: 0, look_only: false, started: true, complete: true, can_stop: false, send: SendMode::Send }
    }

    #[rustfmt::skip]
    fn said(phase: Phase, changed: u32, started: bool) -> String {
        let counts = Counts { read: 0, changed, unsure: 0 };
        let send = SendMode::Send;
        status_text(&StatusRow { phase, counts, steps: 0, look_only: false, started, complete: true, can_stop: false, send })
    }

    #[test]
    fn the_status_sentences_read_as_the_board_does() {
        let (working, done) = (Phase::Working, Phase::Idle);
        assert_eq!(
            said(working, 0, true),
            "Working. Nothing has been changed yet."
        );
        assert_eq!(said(working, 1, true), "Working. 1 change made so far.");
        assert_eq!(said(working, 3, true), "Working. 3 changes made so far.");
        assert_eq!(said(done, 0, true), "Done. Nothing was changed.");
        assert_eq!(said(done, 1, true), "Done. 1 change was made.");
        assert_eq!(said(done, 3, true), "Done. 3 changes were made.");
        assert_eq!(said(done, 0, false), "Ready.");
        assert!(said(Phase::WaitingApproval, 0, true).starts_with("Waiting for your OK."));
        assert!(said(Phase::Offline, 0, true).contains("Lost the connection"));
        assert_eq!(said(Phase::Closed, 0, true), "This task has ended.");
    }

    #[test]
    fn a_change_that_may_have_happened_is_never_hidden() {
        let counts = Counts {
            read: 0,
            changed: 0,
            unsure: 1,
        };
        let base = row_for(Phase::Idle);
        let say = |changed, phase| {
            status_text(&StatusRow {
                counts: Counts { changed, ..counts },
                phase,
                ..base
            })
        };
        assert_eq!(
            say(0, Phase::Idle),
            "Done. Something may have been changed."
        );
        assert_eq!(
            say(2, Phase::Idle),
            "Done. 2 changes made. Something else may have been changed."
        );
        assert!(
            say(2, Phase::Working).starts_with("Working. 2 changes made so far. Something else")
        );
        assert_eq!(
            say(0, Phase::Resuming),
            "Continuing after your OK. Something may have been changed."
        );
    }

    #[test]
    fn headers_counts_and_chips_use_the_board_wording() {
        assert_eq!(group_header(3, true), "Working · 3 steps");
        assert_eq!(group_header(1, false), "Worked · 1 step");
        assert_eq!(
            counts_label(Counts {
                read: 38,
                changed: 0,
                unsure: 0
            }),
            "38 read · 0 changed"
        );
        assert_eq!(look_only_chip(false), "Look only: off");
    }

    #[test]
    fn steps_read_in_plain_words_and_the_hint_follows_the_mode() {
        let step = |tool: &str, target: Option<&str>| Step {
            id: "e".into(),
            tool: tool.into(),
            target: target.map(String::from),
            effect: tool_effect(tool),
            state: StepState::Done,
        };
        assert_eq!(step_label(&step("read_file", Some("a.pdf"))), "Read a.pdf");
        assert_eq!(
            step_label(&step("run_command", Some("rm -rf x"))),
            "Ran a program"
        );
        assert_eq!(step_label(&step("web_fetch", None)), "Looked something up");
        assert_eq!(step_label(&step("mcp_x", None)), "Used mcp_x");
        assert_ne!(
            composer_placeholder(SendMode::Send),
            composer_placeholder(SendMode::Steer)
        );
        assert!(attach_error_text(AttachError::TooLarge { bytes: 1 }).contains("10 MB"));
    }
    #[test]
    fn step_plan_and_mark_words_are_distinct_and_the_failure_is_not_hidden() {
        let steps = [
            StepState::Running,
            StepState::Done,
            StepState::Failed,
            StepState::Unfinished,
        ];
        let words: Vec<_> = steps.iter().map(|s| step_state(*s)).collect();
        assert_eq!(words, ["working", "done", "did not work", "stopped"]);
        assert_ne!(mark_label(Mark::Thinking), mark_label(Mark::Writing));
        let step = Step {
            id: "e".into(),
            tool: "run_command".into(),
            target: Some("mkdir a".into()),
            effect: ToolEffect::Change,
            state: StepState::Done,
        };
        assert_eq!(technical_line(&step), "run_command mkdir a");
        assert_eq!(list_number(12), "12.");
        assert_ne!(GROUP_FOLDED, GROUP_OPEN);
        assert_eq!(images_label(1), "1 image attached");
        assert_eq!(images_label(3), "3 images attached");
        assert!(too_many_tasks(3).starts_with("3 tasks"));
        assert_ne!(send_label(SendMode::Send), send_label(SendMode::Steer));
        let marks = [
            PlanStatus::Pending,
            PlanStatus::InProgress,
            PlanStatus::Completed,
        ];
        assert_eq!(marks.map(plan_mark), ["To do", "Doing", "Done"]);
    }
}
