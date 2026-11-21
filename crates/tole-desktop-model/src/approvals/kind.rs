//! The codes of the shared action summary (#1041), as the model reads them.
//!
//! The server sends `kind`, `effect` and the risk tier as strings. A code this
//! version does not know is read as the most careful value, never a softer one.

/// How much care a card asks for. Ordered: a later value is more careful.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Care {
    Notice,
    Caution,
    Danger,
}

/// The plain-words kind a card is worded for: the `kind` codes of the summary,
/// and "unknown" for any other code. "Outside the folder" is not a kind: it is
/// said on top of the kind, so that card never says less than the same card
/// inside the folder.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Wording {
    CreateFile,
    ReplaceFile,
    DeleteFile,
    RunProgram,
    UseConnectedTool,
    StartDiscussion,
    Delegate,
    Unknown,
}

impl Wording {
    pub fn of(kind: &str) -> Wording {
        match kind {
            "create_file" => Wording::CreateFile,
            "replace_file" => Wording::ReplaceFile,
            "delete_file" => Wording::DeleteFile,
            "run_program" => Wording::RunProgram,
            "use_connected_tool" => Wording::UseConnectedTool,
            "start_discussion" => Wording::StartDiscussion,
            "delegate" => Wording::Delegate,
            _ => Wording::Unknown,
        }
    }

    /// A file action: its preview is a diff and its target is a path.
    pub fn is_file(self) -> bool {
        use Wording::*;
        matches!(self, CreateFile | ReplaceFile | DeleteFile)
    }

    /// The one `effect` code the core pairs with this kind. Any other pair is
    /// not something the core sends: it is read as the most careful wording.
    pub fn expected_effect(self) -> Option<&'static str> {
        match self {
            Wording::CreateFile => Some("adds"),
            Wording::ReplaceFile => Some("replaces_no_backup"),
            Wording::DeleteFile => Some("cannot_be_undone"),
            Wording::RunProgram => Some("runs_program"),
            Wording::UseConnectedTool => Some("uses_connected_tool"),
            Wording::StartDiscussion => Some("starts_discussion"),
            Wording::Delegate => Some("starts_child"),
            Wording::Unknown => None,
        }
    }

    pub fn care(self) -> Care {
        match self {
            Wording::CreateFile => Care::Notice,
            Wording::ReplaceFile
            | Wording::UseConnectedTool
            | Wording::StartDiscussion
            | Wording::Delegate => Care::Caution,
            _ => Care::Danger,
        }
    }
}

/// The care of an `effect` code; an unknown code is [`Care::Danger`].
pub fn effect_care(effect: &str) -> Care {
    match effect {
        "adds" => Care::Notice,
        "replaces_no_backup" | "uses_connected_tool" | "starts_discussion" | "starts_child" => {
            Care::Caution
        }
        _ => Care::Danger,
    }
}

/// The care of a risk tier (`ReadOnly`, `Write`, `Destructive`); an unknown
/// tier, and `Destructive`, are [`Care::Danger`].
pub fn risk_care(risk: &str) -> Care {
    match risk.to_ascii_lowercase().as_str() {
        "readonly" | "write" => Care::Notice,
        _ => Care::Danger,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unknown_codes_are_the_most_careful_value() {
        assert_eq!(Wording::of("teleport"), Wording::Unknown);
        assert_eq!(Wording::of(""), Wording::Unknown);
        assert_eq!(
            (Wording::Unknown.care(), Wording::Unknown.expected_effect()),
            (Care::Danger, None)
        );
        assert_eq!(effect_care("harmless"), Care::Danger);
        assert_eq!(risk_care("safe"), Care::Danger);
        assert_eq!(risk_care("Destructive"), Care::Danger);
        assert_eq!(risk_care("Write"), Care::Notice);
        assert!(Wording::of("delete_file").is_file() && !Wording::of("run_program").is_file());
    }
}
