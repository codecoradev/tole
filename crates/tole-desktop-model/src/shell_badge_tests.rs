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
    // No other row gains a mark.
    assert!(!s.badge_unknown(Section::Automations));
    assert!(!s.badge_unknown(Section::NewTask));
}
