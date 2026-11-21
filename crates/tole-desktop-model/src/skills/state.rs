//! The Skills and tools page: two tabs, each loaded from its own read-only
//! route (`GET /skills`, `GET /tools`).

use super::skill::{RawArchived, RawDraft, RawSkill, SkillsView};
use super::tool::{RawTool, ToolRow, ToolTrust};
use crate::plain::{printable, Load};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Tab {
    #[default]
    Skills,
    ConnectedTools,
}

/// The page state. A tab whose last load failed shows no rows: a list that may
/// be out of date is never drawn as the current trust state.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SkillsPageState {
    pub tab: Tab,
    pub skills_load: Load,
    pub skills: SkillsView,
    pub tools_load: Load,
    pub tools: Vec<ToolRow>,
    pub tools_notes: Vec<String>,
    /// Always false from serve: "trusted" is the saved approval matching, and
    /// the live check starts the server, which only a terminal does.
    pub tools_probed: bool,
}

impl SkillsPageState {
    pub fn select_tab(&mut self, tab: Tab) {
        self.tab = tab;
    }

    pub fn skills_loaded(
        &mut self,
        skills: Vec<RawSkill>,
        drafts: Vec<RawDraft>,
        archived: Vec<RawArchived>,
        notes: Vec<String>,
    ) {
        self.skills = SkillsView::new(skills, drafts, archived, notes);
        self.skills_load = Load::Ready;
    }

    pub fn skills_failed(&mut self, message: &str) {
        self.skills = SkillsView::default();
        self.skills_load = Load::Failed(printable(message, 200));
    }

    pub fn tools_loaded(&mut self, tools: Vec<RawTool>, notes: Vec<String>, probed: bool) {
        self.tools = tools.into_iter().map(ToolRow::new).collect();
        self.tools_notes = notes.iter().map(|n| printable(n, 200)).collect();
        self.tools_probed = probed;
        self.tools_load = Load::Ready;
    }

    pub fn tools_failed(&mut self, message: &str) {
        self.tools.clear();
        self.tools_notes.clear();
        self.tools_probed = false;
        self.tools_load = Load::Failed(printable(message, 200));
    }

    /// The badge of the Skills tab and of the sidebar: what waits for a
    /// decision, or `Unknown` while that cannot be said: nothing loaded yet, a
    /// load that failed, or an answer the server said it could only read in
    /// part. A zero in its place would read as "nothing to review".
    pub fn skills_waiting(&self) -> Badge {
        if self.skills_load == Load::Ready && !self.skills.is_partial() {
            Badge::Count(self.skills.review.len())
        } else {
            Badge::Unknown
        }
    }

    /// The badge of the Connected tools tab: tools that are changed or not
    /// trusted yet (a removed one needs nothing), or `Unknown` as above (a
    /// note from the server means it could not read every tool).
    pub fn tools_needing_attention(&self) -> Badge {
        if self.tools_load == Load::Ready && self.tools_notes.is_empty() {
            Badge::Count(
                self.tools
                    .iter()
                    .filter(|t| matches!(t.trust, ToolTrust::Changed | ToolTrust::NotTrusted))
                    .count(),
            )
        } else {
            Badge::Unknown
        }
    }

    /// The one question a page asks before it says "nothing to review": the
    /// skills were read completely and none waits for a decision.
    pub fn skills_inbox_is_empty(&self) -> bool {
        self.skills_waiting() == Badge::Count(0)
    }

    /// The same question for the Connected tools tab: the tools were read
    /// completely and there are none. An empty list while the load is idle,
    /// running, failed or partial is not "no connected tools".
    pub fn no_connected_tools(&self) -> bool {
        self.tools_load == Load::Ready && self.tools_notes.is_empty() && self.tools.is_empty()
    }
}

/// What a badge says: a number, or that it cannot be known now. A failed,
/// partial or missing load is never a zero.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Badge {
    Count(usize),
    Unknown,
}

impl Badge {
    /// The number to draw beside the label: only a real count above zero. It
    /// is `None` both for a real zero and for `Unknown`, so a view that must
    /// tell "nothing to review" from "not known" matches on the `Badge` itself
    /// (or asks `skills_inbox_is_empty` / `no_connected_tools`).
    pub fn shown(self) -> Option<usize> {
        match self {
            Badge::Count(n) if n > 0 => Some(n),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tool(name: &str, word: &str, state: &str) -> RawTool {
        RawTool {
            trust: word.into(),
            name: name.into(),
            state: state.into(),
            tier: "user".into(),
            source: None,
            version: None,
            approved_at: None,
            secrets: vec![],
            sandbox: "none".into(),
        }
    }

    #[test]
    fn the_badges_count_what_needs_a_decision() {
        let mut s = SkillsPageState::default();
        s.tools_loaded(
            vec![
                tool("cora", "trusted", "approved"),
                tool("uteke", "changed", "changed (plugin manifest)"),
                tool("paper", "not_trusted", "unapproved"),
                tool("old", "removed", "revoked"),
            ],
            vec![],
            false,
        );
        assert_eq!(s.tools_needing_attention(), Badge::Count(2));
        assert_eq!(s.tools_load, Load::Ready);
    }

    /// A badge of 0 means "nothing to review" only when the load really said
    /// so: never loaded, failed and partial reads are `Unknown`.
    #[test]
    fn a_failed_partial_or_missing_load_is_unknown_never_zero() {
        let mut s = SkillsPageState::default();
        assert_eq!(s.skills_waiting(), Badge::Unknown, "nothing loaded yet");
        assert_eq!(s.tools_needing_attention(), Badge::Unknown);
        assert!(!s.skills_inbox_is_empty());

        s.skills_loaded(vec![], vec![], vec![], vec![]);
        assert_eq!(s.skills_waiting(), Badge::Count(0));
        assert!(s.skills_inbox_is_empty());
        assert_eq!(
            s.skills_waiting().shown(),
            None,
            "a real zero draws nothing"
        );

        s.skills_loaded(vec![], vec![], vec![], vec!["skills not read: x".into()]);
        assert_eq!(s.skills_waiting(), Badge::Unknown, "a partial read");
        assert!(!s.skills_inbox_is_empty());

        s.skills_failed("no route");
        assert_eq!(s.skills_waiting(), Badge::Unknown, "a failed load");
        assert!(!s.skills_inbox_is_empty());

        s.tools_loaded(vec![], vec!["plugins not read: y".into()], false);
        assert_eq!(
            s.tools_needing_attention(),
            Badge::Unknown,
            "a partial read"
        );
        s.tools_failed("not reachable");
        assert_eq!(s.tools_needing_attention(), Badge::Unknown, "a failed load");
        s.tools_loaded(vec![], vec![], false);
        assert_eq!(s.tools_needing_attention(), Badge::Count(0));
    }

    /// An empty tools list means "no connected tools" only after a complete
    /// read: idle, failed and partial states never say it.
    #[test]
    fn no_connected_tools_needs_a_complete_empty_read() {
        let mut s = SkillsPageState::default();
        assert!(!s.no_connected_tools(), "nothing loaded yet");
        s.tools_loaded(vec![], vec![], false);
        assert!(s.no_connected_tools());
        s.tools_loaded(vec![], vec!["plugins not read: y".into()], false);
        assert!(!s.no_connected_tools(), "a partial read");
        s.tools_failed("not reachable");
        assert!(!s.no_connected_tools(), "a failed load");
        s.tools_loaded(vec![tool("cora", "trusted", "approved")], vec![], false);
        assert!(!s.no_connected_tools(), "there is one");
    }

    #[test]
    fn a_failed_load_clears_the_rows_it_could_not_refresh() {
        let mut s = SkillsPageState::default();
        s.tools_loaded(vec![tool("cora", "trusted", "approved")], vec![], false);
        s.tools_failed("not reachable");
        assert!(s.tools.is_empty() && s.tools_load == Load::Failed("not reachable".into()));
        s.skills_loaded(vec![], vec![], vec![], vec![]);
        s.skills_failed("no route");
        assert_eq!(s.skills_waiting(), Badge::Unknown);
        assert!(matches!(s.skills_load, Load::Failed(_)));
        s.select_tab(Tab::ConnectedTools);
        assert_eq!(s.tab, Tab::ConnectedTools);
    }
}
