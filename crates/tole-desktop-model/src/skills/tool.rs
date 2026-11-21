//! The Connected tools tab: the trust state of each server.
//!
//! The word comes from the serve view (`trust`: `trusted`, `changed`,
//! `not_trusted`, `removed`), and is closed on the safe side twice: an unknown
//! word is `NotTrusted`, and `Trusted` needs the state sentence of
//! `tole plugin list` to agree, because that sentence is what the lock says.

use crate::plain::printable;

/// The trust of a connected tool.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolTrust {
    /// The lock admits it as it is saved.
    Trusted,
    /// Approved once; something differs now, so Tole does not use it.
    Changed,
    /// Never approved, or not readable, or anything else not known.
    NotTrusted,
    /// Revoked, or approved but no longer configured.
    Removed,
}

/// What differs in a tool that changed since it was trusted, from the state
/// sentence (`changed (another command or URL)`, ...). The route starts no
/// server, so the names of new tools are not known here; the live check is
/// `tole plugin list` at a terminal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToolChange {
    /// Another command or URL.
    Command,
    ProgramFile,
    Manifest,
    SandboxProfile,
    /// Said only "changed".
    Unspecified,
    /// A reason this model does not know, in the server's words.
    Other(String),
}

/// A server of `GET /tools` (mapping of the client's `ToolServer`: `trust`,
/// `name`, `state`, `tier`, `source`, `version`, `approved_at`, `secrets`,
/// `sandbox`; the hashes and `lock` stay at the terminal).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RawTool {
    pub trust: String,
    pub name: String,
    pub state: String,
    pub tier: String,
    pub source: Option<String>,
    pub version: Option<String>,
    pub approved_at: Option<String>,
    pub secrets: Vec<String>,
    pub sandbox: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolRow {
    pub name: String,
    pub trust: ToolTrust,
    /// Set only for a tool that is `Changed`.
    pub change: Option<ToolChange>,
    /// The sentence `tole plugin list` prints, for "Show technical details".
    pub state: String,
    pub tier: String,
    pub source: Option<String>,
    pub version: Option<String>,
    pub approved_at: Option<String>,
    /// Names of the secrets the tool may be given.
    pub secrets: Vec<String>,
    pub sandbox: String,
}

impl ToolRow {
    pub fn new(raw: RawTool) -> ToolRow {
        let state = raw.state.trim();
        let trust = match (raw.trust.as_str(), sentence_trust(state)) {
            ("trusted", ToolTrust::Trusted) => ToolTrust::Trusted,
            // The word said trusted and the lock's sentence did not: the
            // less trusted reading wins.
            ("trusted", other) => other,
            ("changed", _) => ToolTrust::Changed,
            ("removed", _) => ToolTrust::Removed,
            _ => ToolTrust::NotTrusted,
        };
        let change = (trust == ToolTrust::Changed).then(|| change_of(state));
        let text = |s: &str, n| printable(s, n);
        ToolRow {
            name: text(&raw.name, 80),
            trust,
            change,
            state: text(&raw.state, 200),
            tier: text(&raw.tier, 40),
            source: raw.source.map(|s| text(&s, 200)),
            version: raw.version.map(|s| text(&s, 40)),
            approved_at: raw.approved_at.map(|s| text(&s, 40)),
            secrets: raw.secrets.iter().map(|s| text(s, 80)).collect(),
            sandbox: text(&raw.sandbox, 120),
        }
    }
}

/// What the state sentence alone says (the same reading as the serve view's
/// `trust` function in `plugin/view.rs`).
fn sentence_trust(state: &str) -> ToolTrust {
    if state.starts_with("revoked") || state.ends_with("(not configured)") {
        ToolTrust::Removed
    } else if state.starts_with("changed") {
        ToolTrust::Changed
    } else if state == "approved" || state.starts_with("locked (not probed") {
        ToolTrust::Trusted
    } else {
        ToolTrust::NotTrusted
    }
}

fn change_of(state: &str) -> ToolChange {
    let reason = state.strip_prefix("changed").map(str::trim).unwrap_or("");
    match reason {
        "" => ToolChange::Unspecified,
        "(another command or URL)" => ToolChange::Command,
        "(another program file)" => ToolChange::ProgramFile,
        "(plugin manifest)" => ToolChange::Manifest,
        "(sandbox profile)" => ToolChange::SandboxProfile,
        other => ToolChange::Other(printable(other, 80)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn raw(trust: &str, state: &str) -> RawTool {
        RawTool {
            trust: trust.into(),
            name: "uteke".into(),
            state: state.into(),
            tier: "user".into(),
            source: None,
            version: None,
            approved_at: None,
            secrets: vec![],
            sandbox: "none".into(),
        }
    }

    fn trust(word: &str, state: &str) -> ToolTrust {
        ToolRow::new(raw(word, state)).trust
    }

    #[test]
    fn every_sentence_the_list_gives_has_its_trust() {
        for (word, state, want) in [
            ("trusted", "approved", ToolTrust::Trusted),
            (
                "trusted",
                "locked (not probed: this view starts no server)",
                ToolTrust::Trusted,
            ),
            ("changed", "changed (plugin manifest)", ToolTrust::Changed),
            ("not_trusted", "unapproved", ToolTrust::NotTrusted),
            ("removed", "revoked", ToolTrust::Removed),
            ("removed", "approved (not configured)", ToolTrust::Removed),
        ] {
            assert_eq!(trust(word, state), want, "{word}/{state}");
        }
    }

    /// The safe direction is closed: only the word and the sentence together
    /// make a tool trusted.
    #[test]
    fn a_tool_is_never_trusted_unless_word_and_sentence_agree() {
        for word in ["", "TRUSTED", "approved", "ok", "trusted ", "future_word"] {
            assert_eq!(trust(word, "approved"), ToolTrust::NotTrusted, "{word:?}");
        }
        assert_eq!(
            trust("trusted", "changed (sandbox profile)"),
            ToolTrust::Changed
        );
        assert_eq!(trust("trusted", "revoked"), ToolTrust::Removed);
        assert_eq!(
            trust("trusted", "approved (not configured)"),
            ToolTrust::Removed
        );
        for state in ["", "unreadable: bad lock", "approved-ish", "locked"] {
            assert_eq!(trust("trusted", state), ToolTrust::NotTrusted, "{state:?}");
        }
    }

    #[test]
    fn a_changed_tool_says_what_changed() {
        let change = |state: &str| ToolRow::new(raw("changed", state)).change;
        assert_eq!(change("changed"), Some(ToolChange::Unspecified));
        assert_eq!(
            change("changed (another command or URL)"),
            Some(ToolChange::Command)
        );
        assert_eq!(
            change("changed (another program file)"),
            Some(ToolChange::ProgramFile)
        );
        assert_eq!(
            change("changed (plugin manifest)"),
            Some(ToolChange::Manifest)
        );
        assert_eq!(
            change("changed (sandbox profile)"),
            Some(ToolChange::SandboxProfile)
        );
        assert!(matches!(
            change("changed (new thing)"),
            Some(ToolChange::Other(_))
        ));
        assert_eq!(ToolRow::new(raw("trusted", "approved")).change, None);
    }
}
