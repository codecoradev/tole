//! Every word of the approval card and the Approvals page (boards S4, S5, S11).
//!
//! Re-exported as `tole_desktop_model::strings::approvals`. Words are chosen by
//! the server's codes (#1041), never by parsing a sentence; they say what the
//! core says in plain words and never less, and an unknown code gets the most
//! careful sentence.

use super::card::Button;
use super::kind::Wording;
use super::request::Outcome;

pub const TAB_WAITING: &str = "Waiting";
pub const TAB_DECIDED: &str = "Decided";
pub const SHOW_DETAILS: &str = "Show technical details";

pub const LABEL_WHERE: &str = "Where";
pub const LABEL_WAITS: &str = "Waits";
pub const LABEL_ALSO_TOLD: &str = "Also told";

pub const WARNING_MODEL_TEXT: &str =
    "Nothing has been saved yet. Tole wrote this text, you did not type it. Read it before you decide.";
pub const FINE_PRINT: &str = "One time only: Allow once covers this request and the next similar one asks again. The first answer wins.";
pub const DELETE_NOTE: &str = "Tole always asks before deleting. This cannot be turned off.";
pub const PREVIEW_CUT: &str =
    "Not all of it is shown. Show technical details has the tool's own text, and says if it is cut too.";
pub const OUTSIDE_SUFFIX: &str = ", outside the working folder";
pub const LIST_OUTSIDE_SUFFIX: &str = " (outside the folder)";
pub const OUTSIDE_WHERE: &str =
    "Outside the working folder, or the server could not confirm it is inside. Be very careful.";
pub const EFFECT_MISMATCH: &str = "Tole cannot tell what this will do: the kind of request and its effect do not match. Read the details before you decide.";
pub const ANSWERING: &str = "Sending your answer";
pub const EXPIRED_NOTE: &str = "The time ran out. It counted as Don't allow.";
pub const CLOSED_NOTE: &str =
    "Tole is no longer waiting for an answer, and this app does not know how it ended.";

pub const ANSWER_LOST_NOTE: &str = "The time ran out before your answer was confirmed. It may not have arrived. It counts as Don't allow unless the server says otherwise.";

pub const COMPOSER_WAITING_HINT: &str = "Answer the request above to continue...";
/// The box of a task whose requests are not known yet: it never claims that
/// nothing waits.
pub const COMPOSER_CHECKING_HINT: &str = "Checking whether Tole is waiting for you...";

pub const NOT_REPORTED: &str = "Not reported by this server";
pub const DETAIL_TOOL: &str = "Tool";
pub const DETAIL_ARGUMENTS: &str = "Details from the tool";
pub const DETAIL_RISK: &str = "Risk level";
pub const DETAIL_FINGERPRINT: &str = "Fingerprint";
pub const DETAIL_KIND_CODE: &str = "Kind code";
pub const DETAIL_EFFECT_CODE: &str = "Effect code";
pub const DETAIL_REQUEST_ID: &str = "Request id";

/// The top bar of the Approvals page, after the count.
pub const EVERY_ANSWER_FINAL: &str = "every answer is final and is recorded with the task";
/// Between the count and the rule in the top bar.
pub const TOP_SEPARATOR: &str = " \u{b7} ";
pub const NOTHING_WAITING: &str = "Nothing is waiting";
pub const CHECKING_WAITING: &str = "Checking what is waiting";
pub const ALL_FOLDERS: &str = "all folders";
/// The mark of a list row whose file is outside the working folder: a chip of
/// its own, so a long title cannot cut it off.
pub const LIST_OUTSIDE_CHIP: &str = "Outside the folder";

pub const EMPTY_WAITING: &str =
    "Nothing is waiting for you. When Tole needs an OK, it shows up here.";
pub const EMPTY_DECIDED: &str =
    "Nothing decided yet. Answers given while this app is open show up here.";
pub const CHECKING: &str = "Checking what is waiting for you...";
pub const UNREACHABLE: &str = "Tole cannot reach its server to check. It keeps trying.";
pub const NO_CARD: &str = "Pick a request to read it.";
pub const NO_CARD_DECIDED: &str = "Pick a request to see how it ended.";
pub const INCOMPLETE_NOTICE: &str =
    "This list may not show everything yet. Tole is checking again.";
pub const PARKED_NOTICE: &str =
    "A request in a task that is not open here can show up a little late. Tole checks every few seconds.";

/// The top bar: the count, then the rule that answers are final.
pub fn top_line(summary: &str) -> String {
    format!("{summary}{TOP_SEPARATOR}{EVERY_ANSWER_FINAL}")
}

/// "3 waiting".
pub fn waiting_count(count: usize) -> String {
    format!("{count} waiting")
}

/// What is certain while the list is not known.
pub fn at_least_waiting(count: usize) -> String {
    format!("At least {count} waiting")
}

/// A tab name with its count: "Waiting 2" (the count may be the unknown mark).
pub fn tab_with_count(name: &str, count: &str) -> String {
    format!("{name} {count}")
}

/// A request the server sent that this app could not read: it may be waiting.
pub fn refused_notice(count: usize) -> String {
    match count {
        1 => "Tole could not read 1 request from its server. It may be waiting for you.".to_owned(),
        n => format!(
            "Tole could not read {n} requests from its server. They may be waiting for you."
        ),
    }
}

pub fn button_label(button: Button) -> &'static str {
    match button {
        Button::DontAllow => "Don't allow",
        Button::AllowOnce => "Allow once",
    }
}

/// The title of the card: "Tole wants to ...".
pub fn title(w: Wording) -> &'static str {
    match w {
        Wording::CreateFile => "Tole wants to create a file",
        Wording::ReplaceFile => "Tole wants to replace a file",
        Wording::DeleteFile => "Tole wants to delete a file",
        Wording::RunProgram => "Tole wants to run a program",
        Wording::UseConnectedTool => "Tole wants to use a connected tool",
        Wording::StartDiscussion => "Tole wants to start a discussion",
        Wording::Delegate => "Tole wants to hand a task to a helper",
        Wording::Unknown => "Tole wants to use a tool it cannot describe",
    }
}

/// The short title of a row of the list.
pub fn list_title(w: Wording) -> &'static str {
    match w {
        Wording::CreateFile => "Create a file",
        Wording::ReplaceFile => "Replace a file",
        Wording::DeleteFile => "Delete a file",
        Wording::RunProgram => "Run a program",
        Wording::UseConnectedTool => "Use a connected tool",
        Wording::StartDiscussion => "Start a discussion",
        Wording::Delegate => "Hand a task to a helper",
        Wording::Unknown => "Use a tool it cannot describe",
    }
}

/// The "Where" line. A file action not confirmed inside the folder says so.
pub fn where_line(w: Wording, outside_folder: bool) -> &'static str {
    match w {
        _ if outside_folder => OUTSIDE_WHERE,
        Wording::CreateFile | Wording::ReplaceFile | Wording::DeleteFile => {
            "Inside the working folder"
        }
        Wording::RunProgram => "On this computer, as you",
        Wording::UseConnectedTool => "Through a connected tool",
        Wording::StartDiscussion | Wording::Delegate | Wording::Unknown => {
            "Not stated. Read the details."
        }
    }
}

/// The sentence for an `effect` code, in plain words (never softer than the
/// core's); an unknown code gets the most careful sentence.
pub fn effect_line(effect: &str) -> &'static str {
    match effect {
        "adds" => "Adds a new file. Nothing is replaced.",
        "replaces_no_backup" => {
            "Replaces a file that already exists. Tole cannot restore the old version."
        }
        "cannot_be_undone" => "Cannot be undone.",
        "runs_program" => "Runs a program that can do anything your account can do.",
        "uses_connected_tool" => {
            "Uses a connected tool. What it does is decided by that tool, not by Tole."
        }
        "starts_discussion" => "Starts a discussion that spends part of your budget.",
        "starts_child" => "Starts a helper that can act on its own.",
        _ => "Tole cannot tell what this will do. Read the details before you decide.",
    }
}

/// The "Waits" line: the time left, and what happens at zero.
pub fn waits_line(secs: u64) -> String {
    if secs == 0 {
        return "Time is up. It counts as Don't allow.".to_owned();
    }
    let left = if secs >= 60 {
        let m = secs.div_ceil(60);
        format!("{m} more {}", if m == 1 { "minute" } else { "minutes" })
    } else {
        format!(
            "{secs} more {}",
            if secs == 1 { "second" } else { "seconds" }
        )
    };
    format!("{left}, then it counts as Don't allow")
}

/// The short time of a list row: "14 min left".
pub fn left_short(secs: u64) -> String {
    match secs {
        0 => "time is up".to_owned(),
        1..=59 => format!("{secs} s left"),
        _ => format!("{} min left", secs.div_ceil(60)),
    }
}

pub fn outcome_label(o: Outcome) -> &'static str {
    match o {
        Outcome::Allowed => "Allowed",
        Outcome::NotAllowed => "Not allowed",
        Outcome::Expired => "Expired",
        Outcome::Closed => "No longer waiting",
    }
}

/// The note under a decided card.
pub fn outcome_note(o: Outcome) -> Option<&'static str> {
    match o {
        Outcome::Expired => Some(EXPIRED_NOTE),
        Outcome::Closed => Some(CLOSED_NOTE),
        Outcome::Allowed | Outcome::NotAllowed => None,
    }
}

pub fn risk_label(risk: &str) -> &'static str {
    match risk.to_ascii_lowercase().as_str() {
        "readonly" => "Only reads",
        "write" => "Changes things (Write)",
        "destructive" => "Cannot be undone (Destructive)",
        _ => "Unknown",
    }
}

pub fn from_helper(node: &str) -> String {
    if node.is_empty() {
        "From a helper Tole started".to_owned()
    } else {
        format!("From a helper Tole started: {node}")
    }
}

pub fn also_told(name: &str) -> String {
    format!("Notification \"{name}\" was sent")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn waits_are_worded_in_minutes_then_seconds() {
        assert_eq!(
            waits_line(840),
            "14 more minutes, then it counts as Don't allow"
        );
        assert_eq!(
            waits_line(60),
            "1 more minute, then it counts as Don't allow"
        );
        assert_eq!(
            waits_line(1),
            "1 more second, then it counts as Don't allow"
        );
        assert_eq!(waits_line(0), "Time is up. It counts as Don't allow.");
        assert_eq!(
            (left_short(840).as_str(), left_short(59).as_str()),
            ("14 min left", "59 s left")
        );
    }

    #[test]
    fn the_counts_of_the_page_are_worded_here() {
        assert_eq!(waiting_count(3), "3 waiting");
        assert_eq!(at_least_waiting(2), "At least 2 waiting");
        assert_eq!(tab_with_count(TAB_WAITING, "2"), "Waiting 2");
        assert!(
            !EVERY_ANSWER_FINAL.contains("History"),
            "that page does not exist yet"
        );
    }

    #[test]
    fn the_careful_sentences_are_not_reassuring() {
        for text in [
            effect_line("teleport"),
            title(Wording::Unknown),
            where_line(Wording::CreateFile, true),
        ] {
            for soft in [
                "safe",
                "harmless",
                "nothing is replaced",
                "inside the working",
            ] {
                assert!(!text.to_lowercase().contains(soft), "{text}");
            }
        }
    }
}
