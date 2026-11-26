//! The app state machine's own surface: construction, the command menu, the
//! picker, notes and the drained outputs. The behaviour is split by subject
//! into the submodules below, all `impl App` blocks over the one state struct
//! defined in the crate root.

use crate::{menu, App, Footer, Item, KeyAction, Keymap, Kind, Out, PickKind, Theme};

impl App {
    pub fn new(footer: Footer) -> Self {
        Self {
            recent: footer.model.iter().cloned().collect(),
            footer,
            ..Self::default()
        }
    }

    pub fn with_theme(footer: Footer, theme: Theme) -> Self {
        Self {
            theme,
            ..Self::new(footer)
        }
    }

    /// Use `keymap` for the rebindable actions (and for the key hints).
    pub fn with_keymap(mut self, keymap: Keymap) -> Self {
        self.transcript.tools_key = keymap.label(KeyAction::ToggleTools);
        self.keymap = keymap;
        self
    }

    /// Command names matching the draft while it is a bare `/word`.
    pub fn menu(&self) -> Vec<&'static str> {
        let t = self.editor.text();
        if self.running || !t.starts_with('/') || t.contains(char::is_whitespace) {
            return Vec::new();
        }
        menu::matching(t)
    }

    pub fn running(&self) -> bool {
        self.running
    }

    /// Lines to commit to scrollback (drains).
    pub fn take_out(&mut self) -> Vec<Out> {
        std::mem::take(&mut self.out)
    }

    pub fn open_picker(&mut self, kind: PickKind, items: Vec<Item>) {
        self.picker = Some(menu::Picker::new(kind, items));
    }

    /// Lines of the open picker (empty when closed).
    pub fn picker_lines(&self) -> Vec<String> {
        self.picker.as_ref().map_or_else(Vec::new, |p| p.lines(80))
    }

    /// Record a model switch for Ctrl+P (most recent first).
    pub fn note_model_chosen(&mut self, id: &str) {
        self.recent.retain(|m| m != id);
        self.recent.insert(0, id.to_string());
    }

    /// The pending `/copy` sequence, if any (drains).
    pub fn take_osc52(&mut self) -> Option<String> {
        self.osc52.take()
    }

    /// A one-line note in the transcript (command feedback).
    pub fn note(&mut self, text: &str) {
        let lines = self.transcript.line(Kind::Note, text);
        self.out.extend(lines);
    }
}
