//! The screens of the Skills and Connected tools pages the offscreen render
//! harness draws, one image each: every state a tab can be in, with the data of
//! the design boards S7 and S8. Plain data, so the list is tested without a
//! window; `scripts/desktop-render.sh` writes one PNG per scene.

use super::skill::{RawArchived, RawDraft, RawSkill};
use super::state::{Badge, SkillsPageState, Tab};
use super::tool::RawTool;
use crate::{Action, ConnectionStatus, Load, Section, ShellState};

/// One image of the page.
#[derive(Debug, Clone)]
pub struct SkillsScene {
    /// File stem of the PNG: lower case, digits and `-`.
    pub name: &'static str,
    /// The design board it is compared with.
    pub board: &'static str,
    /// Keystrokes pressed before the image is taken (`"tab"`).
    pub keys: &'static str,
    /// What the page shows.
    pub page: SkillsPageState,
    /// A call is out now (the first read, or a refresh).
    pub call_out: bool,
}

impl SkillsScene {
    /// The shell around the page: the section open, a connected serve, and the
    /// sidebar count the page's own badges add up to (unknown when either is,
    /// as the app writes it).
    pub fn shell(&self) -> ShellState {
        let mut state = ShellState::placeholder();
        state.apply(Action::SelectSection(Section::SkillsAndTools));
        state.apply(Action::SetConnectionStatus(ConnectionStatus::Connected));
        let count = match (
            self.page.skills_waiting(),
            self.page.tools_needing_attention(),
        ) {
            (Badge::Count(a), Badge::Count(b)) => Badge::Count(a + b),
            _ => Badge::Unknown,
        };
        state.apply(Action::SetSkillsNew(count));
        state
    }
}

const S7: &str = "s7-skills.png";
const S8: &str = "s8-tools.png";

fn skill(name: &str, state: &str, status: &str, source: &str) -> RawSkill {
    RawSkill {
        name: name.into(),
        state: state.into(),
        status: status.into(),
        source_text: source.into(),
        approved_at: Some("2026-11-03".into()),
        hash: Some("sha256:9f2c41ab".into()),
    }
}

fn draft(name: &str, description: &str, tainted: bool, warnings: &[&str]) -> RawDraft {
    RawDraft {
        id: format!("d-{name}"),
        name: name.into(),
        description: description.into(),
        created_at: "2026-11-17".into(),
        hash: "sha256:cd01e7f3".into(),
        tainted,
        warnings: warnings.iter().map(|w| (*w).to_owned()).collect(),
        nearest: Some("report".into()),
    }
}

fn tool(name: &str, trust: &str, state: &str, version: Option<&str>) -> RawTool {
    RawTool {
        trust: trust.into(),
        name: name.into(),
        state: state.into(),
        tier: "user".into(),
        source: Some("npx -y @example/server".into()),
        version: version.map(str::to_owned),
        approved_at: matches!(trust, "trusted" | "changed").then(|| "2026-10-20".into()),
        secrets: if name == "uteke" {
            vec!["UTEKE_TOKEN".into()]
        } else {
            vec![]
        },
        sandbox: "workspace only".into(),
    }
}

fn full_skills() -> SkillsPageState {
    let mut page = SkillsPageState::default();
    page.skills_loaded(
        vec![
            skill("invoice-naming", "changed", "changed since approval", ""),
            skill("meeting-summary", "approved", "approved", "source local"),
            skill("tidy-a-folder", "approved", "approved", "source local"),
            skill(
                "plain-reply",
                "approved",
                "approved, unrecorded in the ledger (loads; approve again to record it)",
                "source shared library, pinned 2026-10-01",
            ),
            skill("odd-one", "refused", "refused: approval ignored", ""),
        ],
        vec![
            draft(
                "weekly-report-format",
                "How to lay out the weekly report in three parts",
                true,
                &["It tells Tole to run a command"],
            ),
            draft(
                "quiet-draft",
                "A draft with nothing to warn about",
                false,
                &[],
            ),
        ],
        vec![
            RawArchived {
                skill: "old-notes".into(),
                at: "2026-11-10".into(),
                reason: "unused for 30 days".into(),
            },
            RawArchived {
                skill: "photo-tags".into(),
                at: "2026-11-12".into(),
                reason: "unused for 30 days".into(),
            },
        ],
        vec![],
    );
    page
}

fn full_tools() -> SkillsPageState {
    let mut page = SkillsPageState::default();
    page.tools_loaded(
        vec![
            tool("cora", "trusted", "approved", Some("0.21.1")),
            tool(
                "uteke",
                "changed",
                "changed (plugin manifest)",
                Some("0.24.1"),
            ),
            tool("paper", "not_trusted", "unapproved", None),
            tool("old-notes", "removed", "revoked", None),
        ],
        vec![],
        false,
    );
    page
}

fn on_tools(mut page: SkillsPageState) -> SkillsPageState {
    page.select_tab(Tab::ConnectedTools);
    page
}

fn scene(
    name: &'static str,
    board: &'static str,
    keys: &'static str,
    page: SkillsPageState,
    call_out: bool,
) -> SkillsScene {
    SkillsScene {
        name,
        board,
        keys,
        page,
        call_out,
    }
}

/// Every state of the two tabs worth a look, in the order a reviewer reads.
pub fn skills_scenes() -> Vec<SkillsScene> {
    let mut empty = SkillsPageState::default();
    empty.skills_loaded(vec![], vec![], vec![], vec![]);
    let mut partial = SkillsPageState::default();
    partial.skills_loaded(
        vec![skill(
            "meeting-summary",
            "approved",
            "approved",
            "source local",
        )],
        vec![],
        vec![],
        vec!["skills not read: the drafts folder is unreadable".into()],
    );
    let mut failed = SkillsPageState::default();
    failed.skills_failed("the serve did not answer (connection refused)");
    let mut empty_tools = SkillsPageState::default();
    empty_tools.tools_loaded(vec![], vec![], false);
    let mut partial_tools = full_tools();
    partial_tools.tools_loaded(
        vec![tool("cora", "trusted", "approved", Some("0.21.1"))],
        vec!["plugins not read: the lock file is unreadable".into()],
        false,
    );
    let mut failed_tools = SkillsPageState::default();
    failed_tools.tools_failed("the serve did not answer (connection refused)");
    let mut refreshing = full_skills();
    refreshing.tools_loaded(vec![], vec![], false);
    // A page that failed but still holds rows: the model clears them on a
    // failure, so this cannot happen today, and the window must not depend on
    // that. It draws the failure and none of the rows.
    let mut failed_with_rows = full_skills();
    failed_with_rows.skills_load = Load::Failed("the serve did not answer".into());
    let mut tools_failed_with_rows = full_tools();
    tools_failed_with_rows.tools_load = Load::Failed("the serve did not answer".into());
    vec![
        scene("skills-tab-review", S7, "", full_skills(), false),
        scene("skills-tab-empty", S7, "", empty, false),
        scene("skills-tab-partial", S7, "", partial, false),
        scene("skills-tab-failed", S7, "", failed, false),
        scene(
            "skills-tab-reading",
            S7,
            "",
            SkillsPageState::default(),
            true,
        ),
        scene("skills-tab-refreshing", S7, "", refreshing, true),
        scene(
            "skills-tab-failed-with-rows",
            S7,
            "",
            failed_with_rows,
            false,
        ),
        scene(
            "tools-tab-failed-with-rows",
            S8,
            "",
            on_tools(tools_failed_with_rows),
            false,
        ),
        scene("tools-tab-table", S8, "", on_tools(full_tools()), false),
        scene("tools-tab-empty", S8, "", on_tools(empty_tools), false),
        scene("tools-tab-partial", S8, "", on_tools(partial_tools), false),
        scene("tools-tab-failed", S8, "", on_tools(failed_tools), false),
        scene(
            "skills-focus-first-tab",
            S7,
            "tab tab tab tab tab tab tab tab tab tab tab",
            full_skills(),
            false,
        ),
        scene(
            "skills-focus-refresh",
            S7,
            "tab tab tab tab tab tab tab tab tab tab tab tab tab",
            full_skills(),
            false,
        ),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::skills::Body;
    use crate::FocusTarget;
    use std::collections::HashSet;

    fn scenes() -> Vec<SkillsScene> {
        skills_scenes()
    }

    #[test]
    fn names_are_unique_and_safe_as_file_stems() {
        let all = scenes();
        let names: HashSet<_> = all.iter().map(|s| s.name).collect();
        assert_eq!(names.len(), all.len());
        for s in &all {
            assert!(
                s.name
                    .chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-'),
                "{}",
                s.name
            );
            assert!(s.board.ends_with(".png"));
            for key in s.keys.split_whitespace() {
                assert!(matches!(key, "tab" | "shift-tab"), "{key}");
            }
        }
    }

    /// Every body a tab can have, on each tab, has a scene (reading and not read
    /// are the same words on a tab that was never asked, so reading covers it).
    #[test]
    fn every_body_of_each_tab_has_a_scene() {
        let all = scenes();
        for tab in [Tab::Skills, Tab::ConnectedTools] {
            for want in [Body::Rows, Body::Failed(String::new())] {
                assert!(
                    all.iter().any(|s| {
                        let got = s.page.body(tab, s.call_out);
                        s.page.tab == tab
                            && std::mem::discriminant(&got) == std::mem::discriminant(&want)
                    }),
                    "{tab:?} {want:?}"
                );
            }
        }
        assert!(all
            .iter()
            .any(|s| s.page.body(Tab::Skills, s.call_out) == Body::Loading));
    }

    /// The states that must never read as "nothing is waiting": a partial
    /// read, a failed one and one still being read; and the one that may.
    #[test]
    fn only_a_complete_empty_read_says_nothing_is_waiting() {
        let all = scenes();
        let by = |name: &str| {
            all.iter()
                .find(|s| s.name == name)
                .expect(name)
                .page
                .clone()
        };
        assert!(by("skills-tab-empty").skills_inbox_is_empty());
        for name in [
            "skills-tab-partial",
            "skills-tab-failed",
            "skills-tab-reading",
        ] {
            assert!(!by(name).skills_inbox_is_empty(), "{name}");
        }
        assert!(by("tools-tab-empty").no_connected_tools());
        for name in ["tools-tab-partial", "tools-tab-failed"] {
            assert!(!by(name).no_connected_tools(), "{name}");
        }
    }

    #[test]
    fn the_board_scenes_hold_what_the_boards_show() {
        let all = scenes();
        let review = &all
            .iter()
            .find(|s| s.name == "skills-tab-review")
            .unwrap()
            .page;
        assert!(review
            .skills
            .review
            .iter()
            .any(|i| i.tainted && i.needs_care()));
        assert!(review.skills.review.iter().any(|i| !i.needs_care()));
        assert!(review.skills.approved.iter().any(|a| a.unrecorded));
        assert!(!review.skills.blocked.is_empty() && !review.skills.archived.is_empty());
        assert_eq!(review.skills_load, Load::Ready);
        let tools = &all
            .iter()
            .find(|s| s.name == "tools-tab-table")
            .unwrap()
            .page;
        assert_eq!(tools.tools.len(), 4);
        assert!(tools.tools.iter().any(|t| t.change.is_some()));
    }

    /// The sidebar count of a scene is the page's own, and never a zero for a
    /// page that is not fully known.
    #[test]
    fn the_sidebar_count_of_a_scene_is_what_its_page_adds_up_to() {
        let all = scenes();
        let shell = |name: &str| all.iter().find(|s| s.name == name).expect(name).shell();
        // A full skills tab and no tools read: not known.
        assert_eq!(shell("skills-tab-review").skills_new, Badge::Unknown);
        let mut both = all
            .iter()
            .find(|s| s.name == "skills-tab-review")
            .unwrap()
            .clone();
        both.page.tools_loaded(vec![], vec![], false);
        assert_eq!(both.shell().skills_new, Badge::Count(3));
        assert_eq!(
            both.shell().badge_text(Section::SkillsAndTools).as_deref(),
            Some("3 new")
        );
    }

    /// The two scenes that hold rows under a failed load do hold rows, and the
    /// model still reads them as failed: they exist to prove the window draws
    /// none of them.
    #[test]
    fn the_failed_scenes_with_rows_have_rows_and_a_failed_body() {
        let all = scenes();
        for (name, tab) in [
            ("skills-tab-failed-with-rows", Tab::Skills),
            ("tools-tab-failed-with-rows", Tab::ConnectedTools),
        ] {
            let s = all.iter().find(|s| s.name == name).expect(name);
            assert!(matches!(s.page.body(tab, false), Body::Failed(_)), "{name}");
            assert_eq!(s.page.tab, tab);
        }
        let skills = &all
            .iter()
            .find(|s| s.name == "skills-tab-failed-with-rows")
            .unwrap()
            .page;
        assert!(!skills.skills.review.is_empty() && !skills.skills.approved.is_empty());
        let tools = &all
            .iter()
            .find(|s| s.name == "tools-tab-failed-with-rows")
            .unwrap()
            .page;
        assert!(!tools.tools.is_empty());
    }

    #[test]
    fn each_focus_scene_presses_exactly_the_tabs_that_reach_its_control() {
        let all = scenes();
        for (name, target) in [
            (
                "skills-focus-first-tab",
                FocusTarget::SkillsTab(Tab::Skills),
            ),
            ("skills-focus-refresh", FocusTarget::SkillsRefresh),
        ] {
            let s = all.iter().find(|s| s.name == name).expect(name);
            let presses = s.keys.split_whitespace().count();
            assert_eq!(
                s.shell().tab_order().get(presses - 1),
                Some(&target),
                "{name}"
            );
        }
    }
}
