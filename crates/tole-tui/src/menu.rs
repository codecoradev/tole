//! Slash commands (#461): the command table, parsing, and the small
//! filter-as-you-type list used by the model and session pickers.

use crate::sanitize::one_line;

/// `(name, one-line help)`; the order is the menu order.
pub const COMMANDS: [(&str, &str); 11] = [
    ("/model", "pick the model (Ctrl+L), or /model <id>"),
    ("/thinking", "set reasoning effort (Shift+Tab cycles)"),
    ("/compact", "summarize earlier turns before the next prompt"),
    ("/new", "start a fresh session"),
    ("/resume", "switch to an earlier session"),
    (
        "/fork",
        "fork this session at a settled entry: /fork [entry]",
    ),
    (
        "/tree",
        "this session's forks and parents; pick one to switch",
    ),
    ("/session", "model, effort, context and token status"),
    ("/name", "name this session: /name <text>"),
    (
        "/image",
        "attach an image to the next message: /image <path> | clear",
    ),
    ("/quit", "quit"),
];

/// The `/thinking` levels, in cycle order (`unset` clears the override).
pub const EFFORTS: [&str; 4] = ["minimal", "low", "medium", "high"];

/// Commands whose name starts with `typed` (a bare `/word`, no arguments).
pub fn matching(typed: &str) -> Vec<&'static str> {
    COMMANDS
        .iter()
        .map(|c| c.0)
        .filter(|n| n.starts_with(typed))
        .collect()
}

/// Next level of the `minimal → low → medium → high → unset` cycle.
pub fn next_effort(current: Option<&str>) -> Option<String> {
    match current.and_then(|c| EFFORTS.iter().position(|e| *e == c)) {
        None if current.is_some_and(|c| !c.is_empty()) => None, // unknown value: back to unset
        None => Some(EFFORTS[0].into()),
        Some(i) => EFFORTS.get(i + 1).map(|e| e.to_string()),
    }
}

/// One selectable row: `value` goes to the host, `label` is shown.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Item {
    pub value: String,
    pub label: String,
}

impl Item {
    pub fn new(value: impl Into<String>, label: impl Into<String>) -> Self {
        Item {
            value: value.into(),
            label: label.into(),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PickKind {
    Model,
    Session,
    /// `/tree` (#463): the fork family; the open session is preselected.
    Tree,
}

/// Rows shown at once (the live area has a header row plus these).
pub const ROWS: usize = 4;

pub struct Picker {
    pub kind: PickKind,
    items: Vec<Item>,
    filter: String,
    sel: usize,
}

impl Picker {
    pub fn new(kind: PickKind, items: Vec<Item>) -> Self {
        Picker {
            kind,
            items,
            filter: String::new(),
            sel: 0,
        }
    }

    fn visible(&self) -> Vec<&Item> {
        let f = self.filter.to_lowercase();
        self.items
            .iter()
            .filter(|i| i.label.to_lowercase().contains(&f) || i.value.to_lowercase().contains(&f))
            .collect()
    }

    pub fn type_char(&mut self, c: char) {
        self.filter.push(c);
        self.sel = 0;
    }

    pub fn backspace(&mut self) {
        self.filter.pop();
        self.sel = 0;
    }

    pub fn up(&mut self) {
        self.sel = self.sel.saturating_sub(1);
    }

    pub fn down(&mut self) {
        self.sel = (self.sel + 1).min(self.visible().len().saturating_sub(1));
    }

    /// Move the selection to the row whose value is `value` (no-op if absent).
    pub fn select(&mut self, value: &str) {
        if let Some(i) = self.visible().iter().position(|i| i.value == value) {
            self.sel = i;
        }
    }

    pub fn chosen(&self) -> Option<String> {
        self.visible().get(self.sel).map(|i| i.value.clone())
    }

    /// Header (the filter) plus a window of rows around the selection.
    /// Everything goes through `one_line`: ids and names come from the network
    /// or from disk.
    pub fn lines(&self, width: usize) -> Vec<String> {
        let what = match self.kind {
            PickKind::Model => "model",
            PickKind::Session => "session",
            PickKind::Tree => "tree",
        };
        let vis = self.visible();
        let mut out = vec![clip(
            &format!(
                "{what}> {}  ({} of {} · Enter picks · Esc closes)",
                one_line(&self.filter),
                vis.len(),
                self.items.len()
            ),
            width,
        )];
        let start = self.sel.saturating_sub(ROWS - 1);
        for (i, it) in vis.iter().enumerate().skip(start).take(ROWS) {
            let mark = if i == self.sel { "▸ " } else { "  " };
            out.push(clip(&format!("{mark}{}", one_line(&it.label)), width));
        }
        out
    }
}

fn clip(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let cut: String = s.chars().take(max.saturating_sub(1)).collect();
    format!("{cut}…")
}

/// Refusal text for an unknown command, with prefix matches as the hint.
pub fn unknown(cmd: &str) -> String {
    let tail = cmd.strip_prefix('/').unwrap_or(cmd);
    let close: Vec<&str> = COMMANDS
        .iter()
        .map(|c| c.0)
        .filter(|n| n.starts_with(cmd) || cmd.starts_with(n) || n.contains(tail))
        .collect();
    let hint = if close.is_empty() {
        "type / to list the commands".to_string()
    } else {
        format!("did you mean {}?", close.join(", "))
    };
    format!("(unknown command {cmd}; {hint})")
}
