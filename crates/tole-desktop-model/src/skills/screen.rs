//! What the Skills and Connected tools pages decide before they draw: which
//! body a tab shows, the number on a tab, the colour of a trust state, and how
//! a fresh page state replaces the one on screen. Plain data and no window, so
//! the rules are tested here and the view only follows them.

use std::hash::{Hash, Hasher};

use super::skill::{ReviewItem, ReviewReason};
use super::state::{Badge, SkillsPageState, Tab};
use super::tool::ToolTrust;
use crate::plain::Load;
use crate::tokens::Tone;

/// A tab is a focus target of the page, so it has to be hashable.
impl Hash for Tab {
    fn hash<H: Hasher>(&self, state: &mut H) {
        std::mem::discriminant(self).hash(state);
    }
}

/// What a tab draws below its heading.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Body {
    /// Nothing was asked yet (the serve is not ready): not a zero.
    NotLoaded,
    /// The first answer is on its way.
    Loading,
    /// The last load failed. No rows are drawn, so a list that may be out of
    /// date is never read as the current trust state. Carries the words.
    Failed(String),
    /// The server answered: draw the rows, their notes, and the empty line
    /// only when the model says the answer was complete and empty.
    Rows,
}

impl Body {
    /// `load` is the tab's load; `call_out` says a call of its route is out now.
    pub fn of(load: &Load, call_out: bool) -> Body {
        match load {
            Load::Ready => Body::Rows,
            Load::Failed(why) => Body::Failed(why.clone()),
            Load::Loading => Body::Loading,
            Load::Idle if call_out => Body::Loading,
            Load::Idle => Body::NotLoaded,
        }
    }
}

impl SkillsPageState {
    /// The body of `tab`; `call_out` as in [`Body::of`].
    pub fn body(&self, tab: Tab, call_out: bool) -> Body {
        match tab {
            Tab::Skills => Body::of(&self.skills_load, call_out),
            Tab::ConnectedTools => Body::of(&self.tools_load, call_out),
        }
    }

    /// The number beside the name of `tab`: what waits for a decision there,
    /// `Unknown` while that cannot be said.
    pub fn tab_badge(&self, tab: Tab) -> Badge {
        match tab {
            Tab::Skills => self.skills_waiting(),
            Tab::ConnectedTools => self.tools_needing_attention(),
        }
    }

    /// Replaces the page with a fresh read of the server and keeps the tab the
    /// owner is looking at: the tab is the window's, not the server's.
    pub fn adopt(&mut self, fresh: SkillsPageState) {
        let tab = self.tab;
        *self = fresh;
        self.select_tab(tab);
    }
}

impl ReviewItem {
    /// The card asks for the owner's care, not just a decision: the session
    /// that wrote it read untrusted text, the checks warned about it, or it was
    /// approved once and its files differ now.
    pub fn needs_care(&self) -> bool {
        self.tainted
            || !self.warnings.is_empty()
            || self.reason == ReviewReason::ChangedSinceApproval
    }
}

/// The colour meaning of a trust state. Only a trusted tool is good; one that
/// changed or was never trusted asks for the owner, and a removed one is quiet.
pub fn trust_tone(trust: ToolTrust) -> Tone {
    match trust {
        ToolTrust::Trusted => Tone::Good,
        ToolTrust::Changed | ToolTrust::NotTrusted => Tone::Attention,
        ToolTrust::Removed => Tone::Neutral,
    }
}

/// The page as the window holds it: the model's page state and whether a call
/// of each route is out now (what Refresh shows). Plain data with no driver in
/// it, so every platform's window can hold one.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SkillsPane {
    pub page: SkillsPageState,
    /// A call of `GET /skills` is out; the rows shown are from the one before.
    pub skills_loading: bool,
    /// The same for `GET /tools`.
    pub tools_loading: bool,
}

impl SkillsPane {
    /// A call of the route `tab` shows is out.
    pub fn call_out(&self, tab: Tab) -> bool {
        match tab {
            Tab::Skills => self.skills_loading,
            Tab::ConnectedTools => self.tools_loading,
        }
    }

    /// A call of either route is out.
    pub fn refreshing(&self) -> bool {
        self.skills_loading || self.tools_loading
    }

    /// Takes the app's latest read and keeps the tab the owner chose.
    pub fn adopt(&mut self, fresh: SkillsPane) {
        let mut page = std::mem::take(&mut self.page);
        page.adopt(fresh.page);
        *self = SkillsPane { page, ..fresh };
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::skills::RawTool;
    use std::collections::HashSet;

    #[test]
    fn nothing_read_yet_is_not_rows_and_not_empty() {
        assert_eq!(Body::of(&Load::Idle, false), Body::NotLoaded);
        assert_eq!(Body::of(&Load::Idle, true), Body::Loading);
        assert_eq!(Body::of(&Load::Loading, false), Body::Loading);
        assert_eq!(Body::of(&Load::Ready, false), Body::Rows);
    }

    /// A failed load shows its words even while the retry is out, and never rows.
    #[test]
    fn a_failed_load_says_why_while_it_retries() {
        let failed = Load::Failed("no route".into());
        for call_out in [false, true] {
            assert_eq!(Body::of(&failed, call_out), Body::Failed("no route".into()));
        }
    }

    #[test]
    fn each_tab_reads_its_own_load() {
        let mut page = SkillsPageState::default();
        page.skills_loaded(vec![], vec![], vec![], vec![]);
        assert_eq!(page.body(Tab::Skills, false), Body::Rows);
        assert_eq!(page.body(Tab::ConnectedTools, false), Body::NotLoaded);
        page.tools_failed("not reachable");
        assert_eq!(
            page.body(Tab::ConnectedTools, false),
            Body::Failed("not reachable".into())
        );
        assert_eq!(page.body(Tab::Skills, false), Body::Rows);
    }

    #[test]
    fn the_tab_number_is_the_models_badge_and_unknown_is_not_zero() {
        let mut page = SkillsPageState::default();
        assert_eq!(page.tab_badge(Tab::Skills), Badge::Unknown);
        assert_eq!(page.tab_badge(Tab::ConnectedTools), Badge::Unknown);
        page.skills_loaded(vec![], vec![], vec![], vec![]);
        page.tools_loaded(vec![tool("a", "not_trusted", "unapproved")], vec![], false);
        assert_eq!(page.tab_badge(Tab::Skills), Badge::Count(0));
        assert_eq!(page.tab_badge(Tab::ConnectedTools), Badge::Count(1));
    }

    #[test]
    fn a_fresh_read_keeps_the_tab_the_owner_chose() {
        let mut shown = SkillsPageState::default();
        shown.select_tab(Tab::ConnectedTools);
        let mut fresh = SkillsPageState::default();
        fresh.skills_loaded(vec![], vec![], vec![], vec![]);
        shown.adopt(fresh);
        assert_eq!(shown.tab, Tab::ConnectedTools);
        assert_eq!(shown.skills_load, Load::Ready, "the rows are the new ones");
    }

    #[test]
    fn only_a_trusted_tool_is_good_and_tabs_hash_apart() {
        for t in [
            ToolTrust::Changed,
            ToolTrust::NotTrusted,
            ToolTrust::Removed,
        ] {
            assert_ne!(trust_tone(t), Tone::Good, "{t:?}");
        }
        assert_eq!(trust_tone(ToolTrust::Trusted), Tone::Good);
        assert_eq!(trust_tone(ToolTrust::NotTrusted), Tone::Attention);
        let tabs: HashSet<_> = [Tab::Skills, Tab::ConnectedTools].into_iter().collect();
        assert_eq!(tabs.len(), 2);
    }

    fn item(reason: ReviewReason) -> ReviewItem {
        ReviewItem {
            name: "weekly".into(),
            reason,
            draft_id: None,
            description: None,
            created_at: None,
            hash: None,
            tainted: false,
            warnings: vec![],
            nearest: None,
        }
    }

    /// A quiet suggestion is only a decision; each of the three signs makes it
    /// a card that asks for care.
    #[test]
    fn a_tainted_warned_or_changed_skill_asks_for_care() {
        assert!(!item(ReviewReason::SuggestedByTole).needs_care());
        assert!(!item(ReviewReason::NotApprovedYet).needs_care());
        assert!(item(ReviewReason::ChangedSinceApproval).needs_care());
        let mut tainted = item(ReviewReason::SuggestedByTole);
        tainted.tainted = true;
        assert!(tainted.needs_care());
        let mut warned = item(ReviewReason::SuggestedByTole);
        warned.warnings.push("mentions a command".into());
        assert!(warned.needs_care());
    }

    fn tool(name: &str, trust: &str, state: &str) -> RawTool {
        RawTool {
            trust: trust.into(),
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
    fn the_pane_says_which_route_has_a_call_out_and_keeps_the_tab_when_it_adopts() {
        let mut shown = SkillsPane::default();
        shown.page.select_tab(Tab::ConnectedTools);
        assert!(!shown.refreshing());
        let mut fresh = SkillsPane {
            skills_loading: true,
            ..SkillsPane::default()
        };
        fresh.page.skills_loaded(vec![], vec![], vec![], vec![]);
        shown.adopt(fresh);
        assert_eq!(shown.page.tab, Tab::ConnectedTools);
        assert_eq!(shown.page.skills_load, Load::Ready);
        assert!(shown.refreshing());
        assert!(shown.call_out(Tab::Skills) && !shown.call_out(Tab::ConnectedTools));
    }
}
