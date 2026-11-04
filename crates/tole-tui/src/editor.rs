//! Multi-line input buffer with submit history. Cursor is always at the end
//! (no in-buffer cursor movement yet).

#[derive(Default)]
pub struct Editor {
    text: String,
    history: Vec<String>,
    /// Index into `history` while browsing with Up/Down; `None` = the draft.
    browsing: Option<usize>,
    draft: String,
}

impl Editor {
    pub fn insert(&mut self, c: char) {
        self.edited();
        self.text.push(c);
    }

    /// Replace the buffer (Tab completion).
    pub fn set(&mut self, text: &str) {
        self.edited();
        self.text = text.to_string();
    }

    pub fn newline(&mut self) {
        self.insert('\n');
    }

    pub fn backspace(&mut self) {
        self.edited();
        self.text.pop();
    }

    /// Take the trimmed buffer into history; blank input yields `None`.
    pub fn submit(&mut self) -> Option<String> {
        let out = self.text.trim().to_string();
        self.text.clear();
        self.browsing = None;
        if out.is_empty() {
            return None;
        }
        if self.history.last() != Some(&out) {
            self.history.push(out.clone());
        }
        Some(out)
    }

    /// Up: older history entry (the first press stashes the draft).
    pub fn prev(&mut self) {
        let i = match self.browsing {
            None if self.history.is_empty() => return,
            None => {
                self.draft = std::mem::take(&mut self.text);
                self.history.len() - 1
            }
            Some(0) => 0,
            Some(i) => i - 1,
        };
        self.browsing = Some(i);
        self.text = self.history[i].clone();
    }

    /// Down: newer entry, then back to the draft.
    pub fn next(&mut self) {
        match self.browsing {
            None => {}
            Some(i) if i + 1 < self.history.len() => {
                self.browsing = Some(i + 1);
                self.text = self.history[i + 1].clone();
            }
            Some(_) => {
                self.browsing = None;
                self.text = std::mem::take(&mut self.draft);
            }
        }
    }

    pub fn lines(&self) -> Vec<&str> {
        self.text.split('\n').collect()
    }

    pub fn text(&self) -> &str {
        &self.text
    }

    /// Typing over a recalled entry turns it into the new draft.
    fn edited(&mut self) {
        self.browsing = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn typed(e: &mut Editor, s: &str) {
        for c in s.chars() {
            if c == '\n' {
                e.newline()
            } else {
                e.insert(c)
            }
        }
    }

    #[test]
    fn multiline_submit_keeps_inner_newlines_and_clears() {
        let mut e = Editor::default();
        typed(&mut e, "a\nb");
        assert_eq!(e.lines(), ["a", "b"]);
        assert_eq!(e.submit().as_deref(), Some("a\nb"));
        assert_eq!(e.text(), "");
    }

    #[test]
    fn blank_submit_is_none_and_not_history() {
        let mut e = Editor::default();
        typed(&mut e, "  \n ");
        assert_eq!(e.submit(), None);
        e.prev();
        assert_eq!(e.text(), "");
    }

    #[test]
    fn history_walks_back_and_forth_and_restores_draft() {
        let mut e = Editor::default();
        typed(&mut e, "one");
        e.submit();
        typed(&mut e, "two");
        e.submit();
        typed(&mut e, "dra");
        e.prev();
        assert_eq!(e.text(), "two");
        e.prev();
        assert_eq!(e.text(), "one");
        e.prev();
        assert_eq!(e.text(), "one");
        e.next();
        assert_eq!(e.text(), "two");
        e.next();
        assert_eq!(e.text(), "dra");
    }

    #[test]
    fn consecutive_duplicates_collapse() {
        let mut e = Editor::default();
        typed(&mut e, "x");
        e.submit();
        typed(&mut e, "x");
        e.submit();
        e.prev();
        e.prev();
        assert_eq!(e.text(), "x");
    }

    #[test]
    fn backspace_edits_recalled_entry() {
        let mut e = Editor::default();
        typed(&mut e, "abc");
        e.submit();
        e.prev();
        e.backspace();
        assert_eq!(e.text(), "ab");
    }
}
