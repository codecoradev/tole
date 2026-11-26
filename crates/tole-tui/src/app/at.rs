//! The `@` file panel: the word under the cursor, the matching files and the
//! insertion of the chosen path.

use crate::{clip, files, one_line, App, LIVE_ROWS};

impl App {
    /// Replace the `@` file list (tests; the loop lists the cwd lazily).
    pub fn set_files(&mut self, files: Vec<String>) {
        self.files = Some(files);
    }

    /// The `@word` being typed (last whitespace-separated word), minus `@`.
    fn at_filter(&self) -> Option<&str> {
        let t = self.editor.text();
        let word = t.rsplit(char::is_whitespace).next().unwrap_or("");
        word.strip_prefix('@')
    }

    /// Files matching the `@word` under the cursor (empty when none/closed).
    pub fn at_matches(&self) -> Vec<&str> {
        match (self.at_filter(), &self.files) {
            (Some(f), Some(files)) => files::matching(files, f),
            _ => Vec::new(),
        }
    }

    /// Panel rows: a window around the selection, sanitized and clipped.
    pub fn at_lines(&self, width: usize) -> Vec<String> {
        let m = self.at_matches();
        let sel = self.at_sel.min(m.len().saturating_sub(1));
        let start = sel.saturating_sub(LIVE_ROWS as usize - 2);
        m.iter()
            .enumerate()
            .skip(start)
            .take(LIVE_ROWS as usize - 1)
            .map(|(i, p)| {
                clip(
                    &format!("{}{}", if i == sel { "▸ " } else { "  " }, one_line(p)),
                    width,
                )
            })
            .collect()
    }

    /// Tab/Enter on the panel: replace the `@word` with the chosen path.
    pub(crate) fn at_insert(&mut self) -> bool {
        let m = self.at_matches();
        if m.is_empty() {
            return false;
        }
        let path = m[self.at_sel.min(m.len() - 1)].to_string();
        let t = self.editor.text();
        let cut = t
            .rfind(char::is_whitespace)
            .map_or(0, |i| i + t[i..].chars().next().map_or(1, char::len_utf8));
        let new = format!("{}{path} ", &t[..cut]);
        self.editor.set(&new);
        self.at_sel = 0;
        true
    }
}
