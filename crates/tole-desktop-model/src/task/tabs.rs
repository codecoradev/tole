//! Which open task the window shows, and the name on its tab.
//!
//! The backend keeps the set of open tasks (at most three, the order they were
//! opened). What the window adds is the choice of which one fills the main
//! area, or none: a section of the sidebar or the home page takes the area
//! back, and the task stays open as a tab. The choice never names a task that
//! is not open ([`Tabs::sync`]).

use super::transcript::{Item, Transcript};
use crate::plain::printable;

/// Longest tab name; a longer one is cut with an ellipsis.
pub const MAX_TITLE_CHARS: usize = 40;

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Tabs {
    shown: Option<String>,
}

impl Tabs {
    /// The task in the main area.
    pub fn shown(&self) -> Option<&str> {
        self.shown.as_deref()
    }

    /// Put `session` in the main area.
    pub fn show(&mut self, session: &str) {
        self.shown = Some(session.to_owned());
    }

    /// A section or the home page takes the main area; the tasks stay open.
    pub fn leave(&mut self) {
        self.shown = None;
    }

    /// The open set changed (`open` is in the order it was opened): a task
    /// that was closed is not shown. Whether anything changed.
    pub fn sync(&mut self, open: &[String]) -> bool {
        if self.shown.as_ref().is_some_and(|s| !open.contains(s)) {
            self.shown = None;
            return true;
        }
        false
    }

    /// The task that takes the area when the shown one is closed: the one
    /// before it in `open`, else the one after, else none.
    pub fn after_close(open: &[String], closed: &str) -> Option<String> {
        let at = open.iter().position(|s| s == closed)?;
        let neighbour = at.checked_sub(1).or(Some(at + 1))?;
        open.get(neighbour).cloned()
    }
}

/// The name of a task: the first words the owner wrote, as a line; `None`
/// while the transcript has none (a new task has no name yet).
pub fn title(transcript: &Transcript) -> Option<String> {
    transcript.items.iter().find_map(|item| match item {
        Item::User {
            text, steer: false, ..
        } => Some(printable(text, MAX_TITLE_CHARS)).filter(|t| !t.is_empty()),
        _ => None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::task::{TaskEvent, UserKind};

    fn ids(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| (*s).to_owned()).collect()
    }

    #[test]
    fn a_closed_task_is_not_shown_and_a_section_takes_the_area_back() {
        let mut t = Tabs::default();
        t.show("s1");
        assert!(!t.sync(&ids(&["s1", "s2"])));
        assert_eq!(t.shown(), Some("s1"));
        assert!(t.sync(&ids(&["s2"])));
        assert_eq!(t.shown(), None);
        t.show("s2");
        t.leave();
        assert_eq!(t.shown(), None);
        assert!(!t.sync(&[]), "nothing shown, nothing to drop");
    }

    #[test]
    fn closing_a_tab_shows_its_neighbour() {
        let open = ids(&["a", "b", "c"]);
        assert_eq!(Tabs::after_close(&open, "c").as_deref(), Some("b"));
        assert_eq!(Tabs::after_close(&open, "a").as_deref(), Some("b"));
        assert_eq!(Tabs::after_close(&ids(&["a"]), "a"), None);
        assert_eq!(Tabs::after_close(&open, "zz"), None);
    }

    #[test]
    fn the_name_is_the_first_prompt_as_a_short_line() {
        let mut t = Transcript::default();
        assert_eq!(title(&t), None);
        let said = |seq, text: &str, kind| TaskEvent::UserMessage {
            seq,
            text: text.into(),
            images: 0,
            kind,
        };
        t.apply(&said(1, "Synthetic note", UserKind::Synthetic));
        t.apply(&said(2, "A note, not a prompt", UserKind::Steer));
        t.apply(&said(
            3,
            "  Put my invoices\ninto folders by month, keep the originals please  ",
            UserKind::Prompt,
        ));
        t.apply(&said(4, "Second", UserKind::Prompt));
        let name = title(&t).expect("a name");
        assert!(name.starts_with("Put my invoices into folders"));
        assert!(name.chars().count() <= MAX_TITLE_CHARS && name.ends_with('\u{2026}'));
    }
}
