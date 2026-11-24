//! The skills row of the sidebar: a count that is not known is a mark, never a zero.

use crate::{Action, Badge, Section, ShellState};

#[test]
fn an_unknown_skills_count_shows_the_mark_and_never_a_zero() {
    let mut s = ShellState::new();
    assert_eq!(s.skills_new, Badge::Unknown, "a new shell");
    assert!(s.badge_unknown(Section::SkillsAndTools));
    assert_eq!(s.badge_text(Section::SkillsAndTools).as_deref(), Some("?"));
    s.apply(Action::SetSkillsNew(Badge::Count(0)));
    assert!(!s.badge_unknown(Section::SkillsAndTools));
    assert_eq!(s.badge_text(Section::SkillsAndTools), None, "a known zero");
    s.apply(Action::SetSkillsNew(Badge::Count(3)));
    assert_eq!(
        s.badge_text(Section::SkillsAndTools).as_deref(),
        Some("3 new")
    );
    s.apply(Action::SetSkillsNew(Badge::Unknown));
    assert_eq!(s.badge_text(Section::SkillsAndTools).as_deref(), Some("?"));
    // No other row gains a mark of its own: the discussions and the rows with no
    // count never read as unknown.
    assert!(!s.badge_unknown(Section::Discussions));
    assert!(!s.badge_unknown(Section::NewTask));
}

#[test]
fn an_unknown_automations_count_shows_the_mark_and_never_a_zero() {
    let mut s = ShellState::new();
    assert_eq!(s.automations_on, Badge::Unknown, "a new shell");
    assert!(s.badge_unknown(Section::Automations));
    assert_eq!(s.badge_text(Section::Automations).as_deref(), Some("?"));
    s.apply(Action::SetAutomationsOn(Badge::Count(0)));
    assert!(!s.badge_unknown(Section::Automations));
    assert_eq!(s.badge_text(Section::Automations), None, "a known zero");
    s.apply(Action::SetAutomationsOn(Badge::Count(2)));
    assert_eq!(s.badge_text(Section::Automations).as_deref(), Some("2 on"));
    s.apply(Action::SetAutomationsOn(Badge::Unknown));
    assert_eq!(s.badge_text(Section::Automations).as_deref(), Some("?"));
}

#[test]
fn an_unknown_approvals_list_shows_a_mark_not_a_zero() {
    let mut s = ShellState::new();
    assert_eq!(s.approvals_pending, Badge::Unknown, "a new shell");
    assert_eq!(s.badge_text(Section::Approvals).as_deref(), Some("?"));
    s.apply(Action::SetApprovalsPending(Badge::Count(0)));
    assert_eq!(s.badge_text(Section::Approvals), None, "a known zero");
    s.apply(Action::SetApprovalsPending(Badge::Unknown));
    assert_eq!(s.badge(Section::Approvals), None);
    assert!(s.badge_unknown(Section::Approvals));
    assert_eq!(s.badge_text(Section::Approvals).as_deref(), Some("?"));
    assert!(!s.badge_unknown(Section::Discussions));
    s.apply(Action::SetApprovalsPending(Badge::Count(3)));
    assert_eq!(s.badge_text(Section::Approvals).as_deref(), Some("3"));
    s.apply(Action::SetAutomationsOn(Badge::Count(2)));
    assert_eq!(s.badge_text(Section::Automations).as_deref(), Some("2 on"));
}
