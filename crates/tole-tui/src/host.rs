//! The host seam: what the CLI (or a remote adapter) supplies to the UI.
//! [`TuiHost`] and the data types it speaks: the footer, how a turn ended,
//! the slash-command requests ([`HostCmd`]) and the host's data-only answers
//! ([`HostReply`]), the queued image and the session picker row.

use std::sync::Arc;

use tole_core::cancel::CancelToken;
use tole_core::turn::TurnObserver;

use crate::{one_line, Keymap, Theme};

/// Footer fields, composed by the host (it owns the session snapshot).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Footer {
    pub folder: String,
    pub session: String,
    pub model: Option<String>,
    pub effort: Option<String>,
    /// e.g. `context: 41k / 128k (32%)`.
    pub context: String,
    /// e.g. `tokens: 1200 in / 300 out`.
    pub tokens: String,
}

impl Footer {
    /// One line, parts joined with ` · `, cut to `width` chars with `…`.
    pub fn render(&self, width: usize) -> String {
        let parts = [
            self.folder.clone(),
            self.session.clone(),
            self.model.clone().unwrap_or_else(|| "-".into()),
            format!("effort: {}", self.effort.as_deref().unwrap_or("-")),
            self.context.clone(),
            self.tokens.clone(),
        ];
        let line = one_line(&parts.join(" · "));
        if line.chars().count() <= width {
            return line;
        }
        let cut: String = line.chars().take(width.saturating_sub(1)).collect();
        format!("{cut}…")
    }
}

/// How one turn ended, as far as the UI cares.
#[derive(Debug, PartialEq, Eq)]
pub enum TurnEnd {
    Final(String),
    Cancelled,
    /// Any other stop (provider failure, approval denied, ...): shown as a note.
    Stopped(String),
}

/// What the CLI supplies. `Sync`: turns run on a worker thread.
pub trait TuiHost: Sync {
    /// Current footer fields (called at start and after every turn).
    fn footer(&self) -> Footer;
    /// The color theme (#463). Read once at start; the scrollback is
    /// append-only, so it is not switched live. The default is the built-in
    /// `default` theme.
    fn theme(&self) -> Theme {
        Theme::default()
    }
    /// The key bindings (#463). Read once at start. The default is the
    /// built-in keymap (today's keys).
    fn keymap(&self) -> Keymap {
        Keymap::default()
    }
    /// One note line shown at start (for example why a configured theme or
    /// keymap was not used).
    fn startup_note(&self) -> Option<String> {
        None
    }
    /// Run one full turn (blocking). Streamed output goes through `observer`;
    /// `cancel` is set by Esc.
    fn run_turn(
        &self,
        prompt: &str,
        cancel: &CancelToken,
        observer: Arc<dyn TurnObserver>,
    ) -> TurnEnd;

    /// Apply a slash command or hotkey to the session (#461, #526): the one
    /// call a host implements for everything beyond a turn and the footer, so
    /// a remote adapter has three methods to write. Called only while no turn
    /// runs, so a host may take its session lock without blocking the UI. The
    /// reply carries data only; the wording shown to the user is the UI's.
    ///
    /// The default supports nothing but `Status` (no lines) and `Images`
    /// (none pending).
    fn apply(&self, cmd: HostCmd) -> Result<HostReply, String> {
        match cmd {
            HostCmd::Status => Ok(HostReply::Lines(Vec::new())),
            HostCmd::Images => Ok(HostReply::Images(Vec::new())),
            _ => unsupported(),
        }
    }
}

/// What the loop asks of the host for a slash command or hotkey (#461). The
/// host applies it to the session (registers, storage) through
/// [`TuiHost::apply`]; the loop then refreshes the footer, so a switch shows
/// on the next draw.
#[derive(Debug, PartialEq, Eq)]
pub enum HostCmd {
    PickModel,
    SetModel(String),
    /// `None` clears the session override (back to the process default).
    SetEffort(Option<String>),
    /// `/compact [instructions]`.
    Compact(Option<String>),
    New,
    PickSession,
    Switch(String),
    /// `/fork [entry]` (#463): fork the current session at a settled entry
    /// (`None` = the last one) and switch to the fork. Answers `Session`.
    Fork(Option<String>),
    /// `/tree` (#463): the current session's fork family for the picker.
    /// Answers `Sessions`; picking a row sends `Switch`.
    PickTree,
    Status,
    Name(String),
    /// `/image <spec>` or a pasted image path / `data:image/…` URL (#560):
    /// queue an image for the NEXT message. The host resolves `spec` (jail,
    /// size cap, type sniffing) and refuses what it cannot attach.
    AttachImage(String),
    /// `/image clear`: drop every queued image.
    ClearImages,
    /// The images queued for the next message (`/image`, and after a turn
    /// that may have handed them back).
    Images,
    /// `/copy` (#463): the text of the session's last assistant answer.
    LastAnswer,
    /// `/export [path]` (#463): write the transcript to a file (format by
    /// extension, `.html` or `.jsonl`; `None` = the host's default path).
    /// The host jails the path and refuses to overwrite.
    Export(Option<String>),
}

/// What the host answers to a [`HostCmd`] (#526): data, never wording.
#[derive(Debug, PartialEq, Eq)]
pub enum HostReply {
    /// The change was applied (`SetModel`, `SetEffort`, `Compact`, `Name`).
    Done,
    /// The session turns now run against (`New`, `Switch`, `Fork`).
    Session(String),
    /// Model ids for the picker (`PickModel`).
    Models(Vec<String>),
    /// Sessions for the picker (`PickSession`, `PickTree`).
    Sessions(Vec<SessionItem>),
    /// Lines to show (`Status`).
    Lines(Vec<String>),
    /// The images now queued for the next message, in attach order
    /// (`AttachImage`, `ClearImages`, `Images`).
    Images(Vec<ImageItem>),
    /// The last assistant answer, `None` when the session has none
    /// (`LastAnswer`).
    Answer(Option<String>),
    /// Where the transcript was written (`Export`).
    Path(String),
}

/// One image queued for the next message, as the host resolved it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ImageItem {
    pub path: String,
    /// Sniffed from the bytes: `image/png|jpeg|gif|webp`.
    pub mime: String,
    pub bytes: u64,
}

impl ImageItem {
    /// `shot.png (image/png, 12 KiB)`: the file name, not the whole path.
    pub(crate) fn label(&self) -> String {
        let name = self.path.rsplit(['/', '\\']).next().unwrap_or(&self.path);
        let size = match self.bytes {
            b if b < 1024 => format!("{b} B"),
            b if b < 1024 * 1024 => format!("{} KiB", b.div_ceil(1024)),
            b => format!("{:.1} MiB", b as f64 / (1024.0 * 1024.0)),
        };
        one_line(&format!("{name} ({}, {size})", self.mime))
    }
}

/// A past session for the `/resume` picker.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionItem {
    pub id: String,
    pub label: String,
}

pub(crate) fn unsupported<T>() -> Result<T, String> {
    Err("not supported by this host".into())
}
