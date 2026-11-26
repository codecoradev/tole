//! Inline terminal UI for tole: presentation and event loop only.
//!
//! Depends on `tole-core`, never on `tole-cli` (crate cycle); the CLI hands
//! the session over through [`TuiHost`] (it owns provider, storage, config and
//! runs one turn) and the approval prompt through [`approval_channel`].
//! Rendering is a ratatui INLINE viewport (no alternate screen): finished
//! lines are committed to the terminal scrollback, the viewport only holds the
//! live stream line, the editor and the footer.

mod app;
mod approval;
mod clipboard;
mod editor;
mod event_loop;
mod files;
mod guard;
mod host;
mod host_cmd;
mod input;
mod keymap;
#[cfg(test)]
mod keymap_tests;
mod menu;
mod observer;
mod paste;
mod render;
mod sanitize;
mod term_input;
mod theme;
mod transcript;

pub use app::{Action, Msg};
pub use approval::{approval_channel, ApprovalRequest, Pending, Prompter};
pub use editor::Editor;
pub use event_loop::{event_loop, run};
pub use guard::TerminalGuard;
pub use host::{Footer, HostCmd, HostReply, ImageItem, SessionItem, TuiHost, TurnEnd};
pub use input::{Input, Key, KeyPress, Mods};
pub use keymap::{KeyAction, KeySpec, Keymap};
pub use menu::{Item, PickKind};
pub use observer::tool_line;
pub(crate) use sanitize::{clip, one_line, sanitize};
pub use theme::{parse_color, parse_spec, Role, Theme, BUILTIN as BUILTIN_THEMES};
pub use transcript::{Kind, Out, Transcript};

/// live area (5: stream + hint, or a picker/menu) + editor (top border + 4) + footer (1).
const LIVE_ROWS: u16 = 5;
const VIEWPORT_HEIGHT: u16 = LIVE_ROWS + 1 + 4 + 1;
const EDITOR_ROWS: usize = 4;

#[derive(Default)]
pub struct App {
    editor: Editor,
    transcript: Transcript,
    pub footer: Footer,
    armed: bool,
    running: bool,
    saw_text: bool,
    approval: Option<Pending>,
    /// Committed lines not yet written to scrollback.
    out: Vec<Out>,
    /// Set by `Done`: the loop re-reads the host footer.
    pub footer_stale: bool,
    picker: Option<menu::Picker>,
    menu_sel: usize,
    /// Models used in this process, current first (Ctrl+P rotates it).
    recent: Vec<String>,
    /// Follow-ups queued with Alt+Enter while a turn runs (#462).
    queue: Vec<String>,
    /// Steering the turn never picked up (set by the loop at settle).
    leftover: Vec<String>,
    /// Lazily listed files for the `@` panel.
    files: Option<Vec<String>>,
    at_sel: usize,
    /// Images the host has queued for the next message (#560), as last
    /// reported by it.
    images: Vec<ImageItem>,
    /// The running turn took queued images; when it ends the loop asks the
    /// host again (a message that was never recorded hands them back).
    images_in_flight: bool,
    /// Set at the end of such a turn: the loop re-reads the host's queue.
    pub images_stale: bool,
    /// The OSC 52 sequence `/copy` built; the loop writes it to the terminal
    /// (the only escape sequence the UI ever emits) via [`App::take_osc52`].
    osc52: Option<String>,
    /// Colors (#463); the host supplies it, the UI reads no file or env.
    theme: Theme,
    /// Key bindings (#463); the host supplies it, the UI reads no file or env.
    keymap: Keymap,
}

/// Follow-ups that may wait, and queue rows shown above the editor.
const MAX_QUEUE: usize = 16;
const QUEUE_ROWS: usize = 3;

#[cfg(test)]
mod tests;
