//! Words of the task view (board S3). Reached as `strings::task`.
//!
//! Plain words, still a draft like the rest of the table. The rules that pick
//! a sentence live in `crate::task`; these functions turn a state into words.

use crate::task::{
    AttachError, Counts, Phase, SendMode, StatusRow, Step, ToolEffect, MAX_IMAGE_BYTES,
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
        Phase::WaitingApproval => "Waiting for your OK.",
        Phase::Stopping => "Stopping.",
        Phase::Offline => {
            return "Lost the connection to Tole. Reopen the task to try again.".into()
        }
        Phase::Closed => return "This task has ended.".to_string(),
    };
    let n = row.counts.changed;
    let changes = match (n, done) {
        (0, false) => "Nothing has been changed yet.".to_string(),
        (0, true) => "Nothing was changed.".to_string(),
        (_, false) => format!("{} made so far.", plural(n, "change", "changes")),
        (1, true) => "1 change was made.".to_string(),
        (_, true) => format!("{n} changes were made."),
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
        AttachError::NotAnImage => "Tole can read PNG, JPEG, GIF and WebP images.".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::task::{tool_effect, StepState};

    #[rustfmt::skip]
    fn said(phase: Phase, changed: u32, started: bool) -> String {
        let counts = Counts { read: 0, changed };
        let send = SendMode::Send;
        status_text(&StatusRow { phase, counts, steps: 0, look_only: false, started, can_stop: false, send })
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
    fn headers_counts_and_chips_use_the_board_wording() {
        assert_eq!(group_header(3, true), "Working · 3 steps");
        assert_eq!(group_header(1, false), "Worked · 1 step");
        assert_eq!(
            counts_label(Counts {
                read: 38,
                changed: 0
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
}
