//! UI-neutral input (#526): what the app logic needs to know about a key,
//! with no terminal types in it. `App::on_input` consumes this; the
//! crossterm mapping lives in `term_input`, so a second front end (GPUI, a
//! remote client) feeds the same state machine without a terminal.
//!
//! The keys are exactly the ones the logic distinguishes today (the key table
//! in `docs/tui.md` is the behavior contract). Every other key is
//! [`Key::Other`]: it still counts as "a key was pressed" (it disarms a
//! pending Ctrl+C, re-asks an approval) but triggers nothing.

/// A key, independent of any terminal library.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Key {
    Char(char),
    Enter,
    Esc,
    Tab,
    /// Shift+Tab (a distinct key, so Shift+Enter keeps its newline meaning).
    BackTab,
    Backspace,
    Up,
    Down,
    /// Any key the logic does not distinguish.
    Other,
}

/// Held modifiers.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Mods {
    pub ctrl: bool,
    pub alt: bool,
    pub shift: bool,
}

/// One key press with its modifiers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Input {
    pub key: Key,
    pub mods: Mods,
}

impl Input {
    /// `key` with no modifiers.
    pub fn new(key: Key) -> Self {
        Self {
            key,
            mods: Mods::default(),
        }
    }

    pub fn ctrl(key: Key) -> Self {
        Self {
            key,
            mods: Mods {
                ctrl: true,
                ..Mods::default()
            },
        }
    }

    pub fn alt(key: Key) -> Self {
        Self {
            key,
            mods: Mods {
                alt: true,
                ..Mods::default()
            },
        }
    }

    pub fn shift(key: Key) -> Self {
        Self {
            key,
            mods: Mods {
                shift: true,
                ..Mods::default()
            },
        }
    }
}
