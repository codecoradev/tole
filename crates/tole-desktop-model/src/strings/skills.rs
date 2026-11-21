//! Words of the Skills and Connected tools pages (boards S7 and S8). Wording
//! is a draft, as in the rest of [`crate::strings`].

use crate::skills::{BlockedReason, ReviewItem, ReviewReason, ToolChange, ToolTrust};

pub const TITLE: &str = "Skills and tools";
pub const SKILLS_SUBTITLE: &str =
    "Skills are short instructions Tole follows for a kind of task. Tole only uses a skill you approved.";
pub const TOOLS_SUBTITLE: &str =
    "Connected tools are programs and services Tole can use. Tole only uses a tool you trusted.";
pub const TAB_SKILLS: &str = "Skills";
pub const TAB_TOOLS: &str = "Connected tools";
pub const GROUP_REVIEW: &str = "Waiting for your review";
pub const GROUP_APPROVED: &str = "Approved";
pub const GROUP_BLOCKED: &str = "Not used";
pub const GROUP_ARCHIVED: &str = "Archived by the tidy-up";
pub const NOTHING_TO_REVIEW: &str = "Nothing is waiting for your review";
pub const TOOLS_EMPTY: &str = "No connected tools";
pub const LOAD_FAILED: &str = "Tole could not read this page";
pub const TOOLS_LIVE_CHECK: &str =
    "This is the saved approval. To check a tool live, run `tole plugin list` in a terminal.";
pub const TIDY_UP_NOTE: &str =
    "Tidy-up archives skills that Tole created and nobody has used for a long time. It never touches skills you wrote.";
pub const OWNER_ONLY_SKILLS: &str =
    "Tole cannot use a skill until you approve it. A skill Tole suggests is never used automatically.";
pub const OWNER_ONLY_TOOLS: &str = "Tole itself can never trust, install or change a tool.";
/// How the owner approves a skill today (a terminal command).
pub const APPROVE_SKILL_COMMAND: &str = "tole skills approve";
/// How the owner trusts a connected tool today (a terminal command).
pub const APPROVE_TOOL_COMMAND: &str = "tole plugin approve";

/// The column titles of the tools table.
pub const TOOL_COLUMNS: [&str; 2] = ["Tool", "Trust"];

/// The tag on a review row.
pub fn review_tag(reason: ReviewReason) -> &'static str {
    match reason {
        ReviewReason::SuggestedByTole => "Suggested by Tole",
        ReviewReason::ChangedSinceApproval => "Changed",
        ReviewReason::NotApprovedYet => "Not approved yet",
    }
}

/// The second line of a review row.
pub fn review_line(item: &ReviewItem) -> String {
    match item.reason {
        ReviewReason::SuggestedByTole => item
            .description
            .clone()
            .filter(|d| !d.is_empty())
            .unwrap_or_else(|| "Tole suggests this skill.".to_owned()),
        ReviewReason::ChangedSinceApproval => {
            "Changed since you approved it. Tole stopped using it.".to_owned()
        }
        ReviewReason::NotApprovedYet => {
            "Found in a skills folder. Tole does not use it.".to_owned()
        }
    }
}

/// A warning for a draft written in a session that read untrusted text.
pub const TAINTED_WARNING: &str =
    "Tole read text from outside while writing this. Read all of it with care.";

/// Why a skill is not used.
pub fn blocked_reason(reason: &BlockedReason) -> String {
    match reason {
        BlockedReason::Refused => "Its approval is not valid: the record disagrees.".to_owned(),
        BlockedReason::Unreadable => "Tole cannot read this skill.".to_owned(),
        BlockedReason::Unknown(why) => format!("Not used: Tole does not know this state ({why})."),
    }
}

/// The note under an approved skill whose approval has no history record.
pub const UNRECORDED_NOTE: &str =
    "Approved, but not recorded in the history. Approve it again in a terminal to record it.";

/// The trust column, and the words on a trust badge.
pub fn trust_label(trust: ToolTrust) -> &'static str {
    match trust {
        ToolTrust::Trusted => "Trusted",
        ToolTrust::Changed => "Changed since you trusted it",
        ToolTrust::NotTrusted => "Not trusted yet",
        ToolTrust::Removed => "Removed",
    }
}

/// What a trust state means for Tole.
pub fn trust_effect(trust: ToolTrust) -> &'static str {
    match trust {
        ToolTrust::Trusted => "Tole can use this tool.",
        ToolTrust::Changed => {
            "Tole stopped using it. It stays off until you trust this new version."
        }
        ToolTrust::NotTrusted => "Tole does not use it until you trust it.",
        ToolTrust::Removed => "It is no longer connected, or you revoked it.",
    }
}

/// "What is different" for a changed tool.
pub fn change_text(change: &ToolChange) -> String {
    match change {
        ToolChange::Command => "It now starts from another command or web address.".to_owned(),
        ToolChange::ProgramFile => "Its program file is different.".to_owned(),
        ToolChange::Manifest => "Its list of abilities is different.".to_owned(),
        ToolChange::SandboxProfile => "Its safety limits are different.".to_owned(),
        ToolChange::Unspecified => "Something about it is different.".to_owned(),
        ToolChange::Other(why) => format!("Something about it is different: {why}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_trusted_reads_as_trusted() {
        for t in [
            ToolTrust::Changed,
            ToolTrust::NotTrusted,
            ToolTrust::Removed,
        ] {
            assert!(!trust_label(t).starts_with("Trusted"), "{t:?}");
            assert!(!trust_effect(t).contains("can use"), "{t:?}");
        }
        assert_eq!(trust_label(ToolTrust::Trusted), "Trusted");
    }

    #[test]
    fn the_owner_commands_are_the_real_terminal_commands() {
        assert_eq!(APPROVE_SKILL_COMMAND, "tole skills approve");
        assert_eq!(APPROVE_TOOL_COMMAND, "tole plugin approve");
        assert!(change_text(&ToolChange::Other("x".into())).contains('x'));
        assert!(blocked_reason(&BlockedReason::Unknown("odd".into())).contains("odd"));
    }
}
