//! Inline terminal UI for tole: presentation and event loop only.
//!
//! Depends on `tole-core`, never on `tole-cli` (crate cycle); the CLI hands
//! the session over through [`TuiHost`] (it owns provider, storage, config and
//! runs one turn) and the approval prompt through [`approval_channel`].
//! Rendering is a ratatui INLINE viewport (no alternate screen): finished
//! lines are committed to the terminal scrollback, the viewport only holds the
//! live stream line, the editor and the footer.

mod clipboard;
mod editor;
mod files;
mod input;
mod menu;
mod paste;
mod render;
mod sanitize;
mod term_input;
mod theme;
mod transcript;

use std::io::{self, Stdout};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crossterm::event::{
    self, DisableBracketedPaste, EnableBracketedPaste, Event, KeyboardEnhancementFlags,
    PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
};
use crossterm::terminal::{disable_raw_mode, enable_raw_mode};
use crossterm::{execute, queue};
use ratatui::backend::{Backend, CrosstermBackend};
use ratatui::text::Line;
use ratatui::widgets::Paragraph;
use ratatui::{Terminal, TerminalOptions, Viewport};
use serde_json::Value;
use tole_core::approval::Verdict;
use tole_core::cancel::CancelToken;
use tole_core::tool::Risk;
use tole_core::turn::TurnObserver;

pub use editor::Editor;
pub use input::{Input, Key, KeyPress, Mods};
pub use menu::{Item, PickKind};
use render::{styled, wrap};
pub(crate) use sanitize::{one_line, sanitize};
pub use theme::{parse_color, parse_spec, Role, Theme, BUILTIN as BUILTIN_THEMES};
pub use transcript::{Kind, Out, Transcript};

/// live area (5: stream + hint, or a picker/menu) + editor (top border + 4) + footer (1).
const LIVE_ROWS: u16 = 5;
const VIEWPORT_HEIGHT: u16 = LIVE_ROWS + 1 + 4 + 1;
const EDITOR_ROWS: usize = 4;

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
    /// One note line shown at start (for example why a configured theme was
    /// not used).
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
    fn label(&self) -> String {
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

fn unsupported<T>() -> Result<T, String> {
    Err("not supported by this host".into())
}

/// What a UI shows and decides on for one approval, whoever asked (#526): an
/// in-process turn ([`Prompter::ask`]) or a remote source such as a server's
/// `/approvals` queue ([`Prompter::post`]). It carries the risk tier, so the
/// rule that a `Destructive` call is never "allowed always" lives in one place
/// instead of in each UI.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ApprovalRequest {
    text: String,
    risk: Risk,
}

impl ApprovalRequest {
    /// `text` is reduced to one safe line (control characters removed).
    pub fn new(text: impl AsRef<str>, risk: Risk) -> Self {
        Self {
            text: one_line(text.as_ref()),
            risk,
        }
    }

    /// What is being asked, as one line.
    pub fn text(&self) -> &str {
        &self.text
    }

    pub fn risk(&self) -> Risk {
        self.risk
    }

    pub fn is_destructive(&self) -> bool {
        self.risk == Risk::Destructive
    }

    /// Whether a UI may offer to allow this kind of call from now on. Never for
    /// `Destructive`: it is asked every time (the approver never allowlists it).
    pub fn can_allow_always(&self) -> bool {
        !self.is_destructive()
    }
}

/// An approval waiting for an answer: the request plus where the verdict goes.
/// The verdict is delivered exactly once, and **dropping the `Pending`
/// unanswered denies** (fail closed), so a cancelled turn, a vanished UI or a
/// dropped connection can never leave a call allowed.
pub struct Pending {
    request: ApprovalRequest,
    reply: Option<Box<dyn FnOnce(Verdict) + Send>>,
}

impl Pending {
    pub fn new(request: ApprovalRequest, reply: impl FnOnce(Verdict) + Send + 'static) -> Self {
        Self {
            request,
            reply: Some(Box::new(reply)),
        }
    }

    pub fn request(&self) -> &ApprovalRequest {
        &self.request
    }

    /// Answer and consume.
    fn resolve(mut self, verdict: Verdict) {
        if let Some(reply) = self.reply.take() {
            reply(verdict);
        }
    }
}

impl Drop for Pending {
    fn drop(&mut self) {
        if let Some(reply) = self.reply.take() {
            reply(Verdict::Deny);
        }
    }
}

/// Handed to an approval source; puts requests in front of the UI.
#[derive(Clone)]
pub struct Prompter(Arc<Mutex<Sender<Pending>>>);

impl Prompter {
    /// Hand a request to the UI without waiting; `reply` gets the verdict once.
    /// Returns `false` when the UI is gone, in which case `reply` has already
    /// been called with `Deny`.
    pub fn post(
        &self,
        request: ApprovalRequest,
        reply: impl FnOnce(Verdict) + Send + 'static,
    ) -> bool {
        let pending = Pending::new(request, reply);
        self.0
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .send(pending)
            .is_ok()
    }

    /// Ask and block until the UI answers (the in-process turn thread).
    pub fn ask(&self, request: ApprovalRequest) -> Verdict {
        let (tx, rx) = mpsc::channel();
        self.post(request, move |v| {
            let _ = tx.send(v);
        });
        rx.recv().unwrap_or(Verdict::Deny)
    }
}

pub fn approval_channel() -> (Prompter, Receiver<Pending>) {
    let (tx, rx) = mpsc::channel();
    (Prompter(Arc::new(Mutex::new(tx))), rx)
}

/// Observer callbacks -> UI messages.
struct ChanObserver(Mutex<Sender<Msg>>);

impl ChanObserver {
    fn send(&self, m: Msg) {
        let _ = self.0.lock().unwrap_or_else(|p| p.into_inner()).send(m);
    }
}

/// `▸ write_file out.txt`: the tool plus its first short string argument.
pub fn tool_line(tool: &str, input: &Value) -> String {
    let arg = input
        .as_object()
        .and_then(|o| o.values().find_map(Value::as_str))
        .map(|s| clip(&one_line(s.lines().next().unwrap_or("")), 60))
        .unwrap_or_default();
    format!("▸ {tool} {arg}").trim_end().to_string()
}

fn clip(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let cut: String = s.chars().take(max).collect();
    format!("{cut}…")
}

impl TurnObserver for ChanObserver {
    fn tool_started(&self, tool: &str, input: &Value) {
        self.send(Msg::Tool(tool_line(tool, input)));
    }
    fn tool_finished(&self, _tool: &str, ok: bool, preview: &str) {
        self.send(Msg::ToolResult {
            ok,
            preview: preview.to_string(),
        });
    }
    fn reasoning(&self, text: &str) {
        self.send(Msg::Reasoning(text.to_string()));
    }
    fn text_delta(&self, text: &str) {
        self.send(Msg::Text(text.to_string()));
    }
}

pub enum Msg {
    Text(String),
    Reasoning(String),
    Tool(String),
    /// A finished tool call; collapsing/diff styling happen in the transcript.
    ToolResult {
        ok: bool,
        preview: String,
    },
    Approval(Pending),
    Done(TurnEnd),
}

/// What a key press asks of the loop.
#[derive(Debug, PartialEq, Eq)]
pub enum Action {
    None,
    Redraw,
    Quit,
    Submit(String),
    /// Enter while a turn runs: a soft interrupt (#377) for the running turn.
    Steer(String),
    Cancel,
    /// A slash command or hotkey for the host; the loop runs it.
    Host(HostCmd),
    /// A paste that is exactly one image reference (#560): the loop asks the
    /// host to attach `spec`; if the host refuses, `text` is inserted into the
    /// editor as an ordinary paste.
    PasteImage {
        spec: String,
        text: String,
    },
}

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
}

/// Follow-ups that may wait, and queue rows shown above the editor.
const MAX_QUEUE: usize = 16;
const QUEUE_ROWS: usize = 3;

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

    /// Command names matching the draft while it is a bare `/word`.
    pub fn menu(&self) -> Vec<&'static str> {
        let t = self.editor.text();
        if self.running || !t.starts_with('/') || t.contains(char::is_whitespace) {
            return Vec::new();
        }
        menu::matching(t)
    }

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
    fn at_insert(&mut self) -> bool {
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

    /// Compact rows of the follow-up queue (empty when nothing waits).
    pub fn queue_rows(&self, width: usize) -> Vec<String> {
        self.queue_rows_max(width, QUEUE_ROWS)
    }

    fn queue_rows_max(&self, width: usize, max: usize) -> Vec<String> {
        if max == 0 {
            return Vec::new();
        }
        let mut rows: Vec<String> = self
            .queue
            .iter()
            .take(max)
            .map(|q| clip(&format!("queued: {}", one_line(q)), width.saturating_sub(1)))
            .collect();
        if self.queue.len() > max {
            rows[max - 1] = clip(
                &format!("+{} more queued", self.queue.len() - max + 1),
                width.saturating_sub(1),
            );
        }
        rows
    }

    /// The images queued for the next message, as the host last reported.
    pub fn images(&self) -> &[ImageItem] {
        &self.images
    }

    /// Replace the queued-image list with what the host reports.
    pub fn set_images(&mut self, images: Vec<ImageItem>) {
        self.images = images;
    }

    /// The row for queued images (#560), or `None` when nothing is attached.
    pub fn image_row(&self, width: usize) -> Option<String> {
        let row = match self.images.as_slice() {
            [] => return None,
            [one] => format!("image: {} · next message", one.label()),
            many => format!(
                "images ({}): {} · next message",
                many.len(),
                many.iter()
                    .map(ImageItem::label)
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        };
        Some(clip(&row, width.saturating_sub(1)))
    }

    /// Rows above the editor: the image row, then the follow-up queue, at
    /// most `QUEUE_ROWS` together.
    pub fn pending_rows(&self, width: usize) -> Vec<String> {
        let mut rows: Vec<String> = self.image_row(width).into_iter().collect();
        let left = QUEUE_ROWS - rows.len();
        rows.extend(self.queue_rows_max(width, left));
        rows
    }

    /// The host's queue goes with the message that starts a turn.
    fn images_sent(&mut self) {
        self.images_in_flight = !self.images.is_empty();
        self.images.clear();
    }

    /// Put queued (and `extra`) messages back in front of the draft; nothing
    /// is lost on cancel or on a turn that did not end in an answer.
    fn return_queue(&mut self, extra: Vec<String>) {
        let mut back = extra;
        back.append(&mut self.queue);
        self.prepend_draft(back);
    }

    fn prepend_draft(&mut self, back: Vec<String>) {
        if back.is_empty() {
            return;
        }
        let draft = self.editor.text().to_string();
        let mut text = back.join("\n");
        if !draft.is_empty() {
            text = format!("{text}\n{draft}");
        }
        self.editor.set(&text);
    }

    /// The head of the queue, when idle: becomes the next prompt (one at a time).
    pub fn start_queued(&mut self) -> Option<String> {
        if self.running || self.queue.is_empty() {
            return None;
        }
        let text = self.queue.remove(0);
        self.running = true;
        self.saw_text = false;
        self.images_sent();
        let echoed = self.transcript.line(Kind::User, &format!("> {text}"));
        self.out.extend(echoed);
        Some(text)
    }

    /// Steering text the settled turn never consumed (the loop drains it).
    pub fn leftover(&mut self, texts: Vec<String>) {
        self.leftover = texts;
    }

    /// Echo a steering message the turn accepted.
    pub fn steered(&mut self, text: &str) {
        let echoed = self
            .transcript
            .line(Kind::User, &format!("> {text}  [steer]"));
        self.out.extend(echoed);
    }

    /// Give a draft back to the editor (a refused steer).
    pub fn restore_draft(&mut self, text: &str) {
        self.editor.set(text);
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

    fn cycle_effort(&self) -> Action {
        Action::Host(HostCmd::SetEffort(menu::next_effort(
            self.footer.effort.as_deref(),
        )))
    }

    fn cycle_model(&mut self) -> Action {
        if self.recent.len() < 2 {
            self.note("(no other recent model)");
            return Action::Redraw;
        }
        self.recent.rotate_left(1);
        Action::Host(HostCmd::SetModel(self.recent[0].clone()))
    }

    fn picker_key(&mut self, key: Key) -> Action {
        let Some(p) = self.picker.as_mut() else {
            return Action::None;
        };
        match key {
            Key::Esc => self.picker = None,
            Key::Up => p.up(),
            Key::Down => p.down(),
            Key::Backspace => p.backspace(),
            Key::Char(c) => p.type_char(c),
            Key::Enter => {
                let kind = p.kind;
                let chosen = p.chosen();
                self.picker = None;
                return match (kind, chosen) {
                    (PickKind::Model, Some(id)) => {
                        self.note_model_chosen(&id);
                        Action::Host(HostCmd::SetModel(id))
                    }
                    (PickKind::Session, Some(id)) => Action::Host(HostCmd::Switch(id)),
                    // /tree lists the open session too: picking it is a no-op.
                    (PickKind::Tree, Some(id)) if id == self.footer.session => {
                        self.note(&format!("(already in {id})"));
                        Action::Redraw
                    }
                    (PickKind::Tree, Some(id)) => Action::Host(HostCmd::Switch(id)),
                    _ => Action::Redraw,
                };
            }
            _ => return Action::None,
        }
        Action::Redraw
    }

    /// Enter on a `/…` draft: run the command or refuse it with a hint.
    fn command(&mut self, text: &str) -> Action {
        let (cmd, arg) = match text.split_once(char::is_whitespace) {
            Some((c, a)) => (c, a.trim()),
            None => (text, ""),
        };
        let arg_opt = (!arg.is_empty()).then(|| arg.to_string());
        let host = |c| Action::Host(c);
        match cmd {
            "/quit" => Action::Quit,
            "/model" => match arg_opt {
                Some(id) => {
                    self.note_model_chosen(&id);
                    host(HostCmd::SetModel(id))
                }
                None => host(HostCmd::PickModel),
            },
            "/thinking" => match arg {
                "" => self.cycle_effort(),
                "unset" => host(HostCmd::SetEffort(None)),
                l if menu::EFFORTS.contains(&l) => host(HostCmd::SetEffort(Some(l.into()))),
                _ => {
                    self.note("(usage: /thinking [minimal | low | medium | high | unset])");
                    Action::Redraw
                }
            },
            "/compact" => host(HostCmd::Compact(arg_opt)),
            "/new" => host(HostCmd::New),
            "/resume" => host(HostCmd::PickSession),
            "/fork" => host(HostCmd::Fork(arg_opt)),
            "/tree" => host(HostCmd::PickTree),
            "/session" => host(HostCmd::Status),
            "/image" => match arg {
                "" => host(HostCmd::Images),
                "clear" => host(HostCmd::ClearImages),
                spec => host(HostCmd::AttachImage(spec.to_string())),
            },
            "/copy" if arg.is_empty() => host(HostCmd::LastAnswer),
            "/copy" => {
                self.note("(usage: /copy; it takes no argument, /export [path] writes a file)");
                Action::Redraw
            }
            "/export" => host(HostCmd::Export(arg_opt)),
            "/name" => match arg_opt {
                Some(n) => host(HostCmd::Name(n)),
                None => {
                    self.note("(usage: /name <text>)");
                    Action::Redraw
                }
            },
            _ => {
                self.note(&menu::unknown(cmd));
                Action::Redraw
            }
        }
    }

    pub fn running(&self) -> bool {
        self.running
    }

    /// Lines to commit to scrollback (drains).
    pub fn take_out(&mut self) -> Vec<Out> {
        std::mem::take(&mut self.out)
    }

    /// One input (UI-neutral, #526): a key press or a paste. The terminal
    /// adapter in `term_input` maps crossterm events onto this.
    pub fn on_input(&mut self, input: Input) -> Action {
        match input {
            Input::Key(key) => self.on_key_press(key),
            Input::Paste(text) => self.on_paste(&text),
        }
    }

    /// A pasted block (#560). It never submits, steers or answers anything:
    /// at an approval prompt it is ignored, in a picker it filters, otherwise
    /// it lands in the editor as one edit — unless, while idle, it is exactly
    /// one image reference, which the loop offers to the host to attach.
    fn on_paste(&mut self, raw: &str) -> Action {
        self.armed = false;
        if self.approval.is_some() {
            return Action::None;
        }
        let text = paste::normalize(raw);
        if let Some(p) = self.picker.as_mut() {
            for c in text.chars().filter(|c| !c.is_control()) {
                p.type_char(c);
            }
            return Action::Redraw;
        }
        if text.is_empty() {
            return Action::None;
        }
        if !self.running {
            if let Some(spec) = paste::image_spec(&text) {
                return Action::PasteImage { spec, text };
            }
        }
        self.paste_text(&text);
        Action::Redraw
    }

    /// Insert an (already normalized) paste into the editor as one edit.
    pub fn paste_text(&mut self, text: &str) {
        self.menu_sel = 0;
        self.at_sel = 0;
        self.editor.paste(text);
    }

    fn on_key_press(&mut self, key: KeyPress) -> Action {
        let ctrl = key.mods.ctrl;
        if ctrl && key.key == Key::Char('c') {
            if self.running {
                return self.cancel();
            }
            if self.armed {
                return Action::Quit;
            }
            self.armed = true;
            return Action::Redraw;
        }
        self.armed = false;
        if let Some(p) = self.approval.take() {
            return self.answer(p, key.key);
        }
        if self.picker.is_some() {
            return self.picker_key(key.key);
        }
        // Hotkeys wait for idle (they take the session lock). Shift+Tab is
        // `BackTab`, a different key code from Enter, so Shift+Enter keeps
        // its newline meaning below.
        match key.key {
            Key::Char('o') if ctrl => {
                let mut v = self.transcript.toggle_tools();
                let state = if self.transcript.expand_tools {
                    "expanded"
                } else {
                    "collapsed"
                };
                v.extend(
                    self.transcript
                        .line(Kind::Note, &format!("(tool output {state})")),
                );
                self.out.extend(v);
                return Action::Redraw;
            }
            Key::Char('t') if ctrl => {
                let mut v = self.transcript.toggle_reasoning();
                let state = if self.transcript.hide_reasoning {
                    "hidden"
                } else {
                    "shown"
                };
                v.extend(
                    self.transcript
                        .line(Kind::Note, &format!("(reasoning {state})")),
                );
                self.out.extend(v);
                return Action::Redraw;
            }
            Key::Char('l') if ctrl => {
                return if self.running {
                    Action::None
                } else {
                    Action::Host(HostCmd::PickModel)
                }
            }
            Key::Char('p') if ctrl => {
                return if self.running {
                    Action::None
                } else {
                    self.cycle_model()
                }
            }
            Key::BackTab => {
                return if self.running {
                    Action::None
                } else {
                    self.cycle_effort()
                }
            }
            _ => {}
        }
        if !self.at_matches().is_empty() {
            let n = self.at_matches().len();
            match key.key {
                Key::Tab | Key::Enter if self.at_insert() => return Action::Redraw,
                Key::Up if !key.mods.alt => {
                    self.at_sel = self.at_sel.min(n - 1).saturating_sub(1);
                    return Action::Redraw;
                }
                Key::Down => {
                    self.at_sel = (self.at_sel + 1).min(n - 1);
                    return Action::Redraw;
                }
                _ => {}
            }
        }
        if key.key == Key::Up && key.mods.alt {
            return match self.queue.pop() {
                Some(last) => {
                    self.prepend_draft(vec![last]);
                    Action::Redraw
                }
                None => Action::None,
            };
        }
        let menu = self.menu();
        match key.key {
            Key::Tab if !menu.is_empty() => {
                let pick = menu[self.menu_sel.min(menu.len() - 1)];
                self.editor.set(&format!("{pick} "));
                self.menu_sel = 0;
            }
            Key::Up if !menu.is_empty() => self.menu_sel = self.menu_sel.saturating_sub(1),
            Key::Down if !menu.is_empty() => {
                self.menu_sel = (self.menu_sel + 1).min(menu.len() - 1)
            }
            Key::Esc if self.running => return self.cancel(),
            // Alt+Enter while a turn runs queues a follow-up; idle it is a newline.
            Key::Enter if self.running && key.mods.alt => {
                let t = self.editor.text().trim();
                if t.is_empty() || t.starts_with('/') {
                    return Action::None;
                }
                if self.queue.len() >= MAX_QUEUE {
                    self.note(&format!("({MAX_QUEUE} follow-ups are already queued)"));
                    return Action::Redraw;
                }
                if let Some(text) = self.editor.submit() {
                    self.queue.push(text);
                }
            }
            Key::Enter if key.mods.shift || key.mods.alt => self.editor.newline(),
            Key::Enter => {
                if self.running {
                    // Steer: a non-empty, non-slash draft becomes a soft interrupt.
                    let t = self.editor.text().trim();
                    if t.is_empty() || t.starts_with('/') {
                        return Action::None;
                    }
                    return self.editor.submit().map_or(Action::None, Action::Steer);
                }
                if self.editor.text().trim_start().starts_with('/') {
                    let text = self.editor.submit().unwrap_or_default();
                    self.menu_sel = 0;
                    return self.command(&text);
                }
                return match self.editor.submit() {
                    Some(text) => {
                        self.running = true;
                        self.saw_text = false;
                        self.images_sent();
                        let echoed = self.transcript.line(Kind::User, &format!("> {text}"));
                        self.out.extend(echoed);
                        Action::Submit(text)
                    }
                    None => Action::Redraw,
                };
            }
            // Fallback for terminals that cannot report Shift+Enter.
            Key::Char('j') if ctrl => self.editor.newline(),
            Key::Char(c) if !ctrl => {
                self.menu_sel = 0;
                self.at_sel = 0;
                if c == '@' && self.files.is_none() {
                    self.files = Some(
                        std::env::current_dir().map_or_else(|_| Vec::new(), |d| files::list(&d)),
                    );
                }
                self.editor.insert(c)
            }
            Key::Backspace => {
                self.menu_sel = 0;
                self.at_sel = 0;
                self.editor.backspace()
            }
            Key::Up => self.editor.prev(),
            Key::Down => self.editor.next(),
            _ => return Action::None,
        }
        Action::Redraw
    }

    /// y/n answer; Esc denies AND cancels the turn; anything else re-asks.
    fn answer(&mut self, p: Pending, key: Key) -> Action {
        let (verdict, note) = match key {
            Key::Char('y' | 'Y') => (Verdict::Allow, "allowed"),
            Key::Char('n' | 'N') => (Verdict::Deny, "denied"),
            Key::Esc => {
                p.resolve(Verdict::Deny);
                let lines = self.transcript.line(Kind::Note, "  denied");
                self.out.extend(lines);
                self.return_queue(Vec::new());
                return Action::Cancel;
            }
            _ => {
                self.approval = Some(p);
                return Action::None;
            }
        };
        p.resolve(verdict);
        let lines = self.transcript.line(Kind::Note, &format!("  {note}"));
        self.out.extend(lines);
        Action::Redraw
    }

    fn cancel(&mut self) -> Action {
        // A pending approval is dropped unanswered: the turn thread sees Deny.
        self.approval = None;
        self.return_queue(Vec::new());
        Action::Cancel
    }

    pub fn on_msg(&mut self, m: Msg) {
        let lines = match m {
            Msg::Text(t) => {
                self.saw_text = true;
                self.transcript.push(Kind::Text, &t)
            }
            Msg::Reasoning(t) => self.transcript.push(Kind::Reasoning, &t),
            Msg::Tool(t) => self.transcript.line(Kind::Tool, &t),
            Msg::ToolResult { ok, preview } => self.transcript.tool_result(ok, &preview),
            Msg::Approval(p) => {
                let mut v = self.transcript.flush();
                let tag = if p.request.is_destructive() {
                    "destructive"
                } else {
                    "write"
                };
                v.extend(
                    self.transcript
                        .line(Kind::Note, &format!("? {tag}: {}", p.request.text())),
                );
                self.approval = Some(p);
                v
            }
            Msg::Done(end) => {
                self.running = false;
                self.approval = None;
                self.footer_stale = true;
                self.images_stale = std::mem::take(&mut self.images_in_flight);
                let mut v = self.transcript.flush();
                let left = std::mem::take(&mut self.leftover);
                if matches!(end, TurnEnd::Final(_)) {
                    // steering that arrived too late runs next, ahead of the queue
                    self.queue.splice(0..0, left);
                } else {
                    self.return_queue(left);
                }
                match end {
                    TurnEnd::Final(text) if !self.saw_text => {
                        v.extend(self.transcript.line(Kind::Text, &text))
                    }
                    TurnEnd::Final(_) => {}
                    TurnEnd::Cancelled => {
                        v.extend(self.transcript.line(Kind::Note, "(turn cancelled)"))
                    }
                    TurnEnd::Stopped(m) => {
                        v.extend(self.transcript.line(Kind::Note, &format!("({m})")))
                    }
                }
                v
            }
        };
        self.out.extend(lines);
    }
}

/// Raw mode (+ keyboard enhancement, so Shift+Enter is reported on terminals
/// that speak the kitty protocol; others ignore the sequence; + bracketed
/// paste, #560, so a pasted block arrives as one event instead of keys) for
/// the guard's lifetime; Drop restores all three even when a panic unwinds
/// through `run`.
pub struct TerminalGuard;

impl TerminalGuard {
    pub fn enter() -> io::Result<Self> {
        enable_raw_mode()?;
        let mut out = io::stdout();
        let _ = queue!(
            out,
            PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES),
            EnableBracketedPaste
        );
        let _ = io::Write::flush(&mut out);
        Ok(Self)
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = execute!(
            io::stdout(),
            DisableBracketedPaste,
            PopKeyboardEnhancementFlags
        );
        let _ = disable_raw_mode();
    }
}

/// Run the TUI until quit. SIGINT is a plain `Ctrl+C` key event in raw mode.
pub fn run(host: &dyn TuiHost, approvals: Receiver<Pending>) -> io::Result<()> {
    let _guard = TerminalGuard::enter()?;
    let mut term: Terminal<CrosstermBackend<Stdout>> = Terminal::with_options(
        CrosstermBackend::new(io::stdout()),
        TerminalOptions {
            viewport: Viewport::Inline(VIEWPORT_HEIGHT),
        },
    )?;
    let mut app = App::with_theme(host.footer(), host.theme());
    if let Some(n) = host.startup_note() {
        app.note(&format!("({n})"));
    }
    event_loop(&mut term, host, &mut app, &approvals, |d| {
        if event::poll(d)? {
            event::read().map(Some)
        } else {
            Ok(None)
        }
    })?;
    // Park the cursor under the frame so the shell prompt doesn't overwrite it.
    println!();
    Ok(())
}

fn io_err<B: Backend>(e: B::Error) -> io::Error
where
    B::Error: std::fmt::Display,
{
    io::Error::other(e.to_string())
}

fn commit<B: Backend>(term: &mut Terminal<B>, app: &mut App) -> io::Result<()>
where
    B::Error: std::fmt::Display,
{
    let out = app.take_out();
    if out.is_empty() {
        return Ok(());
    }
    let width = term.size().map_err(io_err::<B>)?.width as usize;
    let theme = &app.theme;
    let lines: Vec<Line> = out
        .iter()
        .flat_map(|o| {
            wrap(&o.text, width)
                .into_iter()
                .map(|t| styled(o.kind, &t, theme))
        })
        .collect();
    let h = lines.len().min(u16::MAX as usize) as u16;
    term.insert_before(h, |buf| {
        Paragraph::new(lines).render(buf.area, buf);
    })
    .map_err(io_err::<B>)
}

use ratatui::widgets::Widget as _;

/// Run a slash command against the host (no turn is running: the app only
/// emits these while idle). Every outcome is a transcript note, and a
/// successful change marks the footer stale so the next loop turn re-reads it.
fn run_host_cmd(host: &dyn TuiHost, app: &mut App, cmd: HostCmd) {
    // What to say, decided before the command is handed over (the host
    // gets it by value and answers with data only).
    #[derive(Clone, Copy)]
    enum Cmd {
        PickModel,
        PickSession,
        Tree,
        Fork,
        New,
        Switch,
        Status,
        Change,
        Attach,
        Clear,
        Images,
        Copy,
        Export,
    }
    let (kind, ok_note) = match &cmd {
        HostCmd::PickModel => (Cmd::PickModel, String::new()),
        HostCmd::PickSession => (Cmd::PickSession, String::new()),
        HostCmd::PickTree => (Cmd::Tree, String::new()),
        HostCmd::Fork(_) => (Cmd::Fork, String::new()),
        HostCmd::New => (Cmd::New, String::new()),
        HostCmd::Switch(_) => (Cmd::Switch, String::new()),
        HostCmd::Status => (Cmd::Status, String::new()),
        HostCmd::SetModel(id) => (Cmd::Change, format!("model → {id}")),
        HostCmd::SetEffort(l) => (
            Cmd::Change,
            format!("thinking → {}", l.as_deref().unwrap_or("unset")),
        ),
        HostCmd::Compact(instructions) => {
            let extra = if instructions.is_some() {
                "; instructions are not supported yet and were ignored"
            } else {
                ""
            };
            (
                Cmd::Change,
                format!("compaction runs before your next prompt{extra}"),
            )
        }
        HostCmd::Name(n) => (Cmd::Change, format!("named {n}")),
        HostCmd::AttachImage(_) => (Cmd::Attach, String::new()),
        HostCmd::ClearImages => (Cmd::Clear, String::new()),
        HostCmd::Images => (Cmd::Images, String::new()),
        HostCmd::LastAnswer => (Cmd::Copy, String::new()),
        HostCmd::Export(_) => (Cmd::Export, String::new()),
    };
    let had = app.images.len();
    let done = |app: &mut App, msg: String| {
        app.footer_stale = true;
        app.note(&format!("({msg})"));
    };
    match (kind, host.apply(cmd)) {
        (Cmd::PickModel, Ok(HostReply::Models(ids))) if ids.is_empty() => {
            app.note("(the provider lists no models; use /model <id>)")
        }
        (Cmd::PickModel, Ok(HostReply::Models(ids))) => app.open_picker(
            PickKind::Model,
            ids.into_iter().map(|i| Item::new(i.clone(), i)).collect(),
        ),
        (Cmd::PickModel, Err(e)) => app.note(&format!("(models: {e}; use /model <id>)")),
        (Cmd::PickSession, Ok(HostReply::Sessions(v))) if v.is_empty() => {
            app.note("(no other sessions)")
        }
        (Cmd::PickSession, Ok(HostReply::Sessions(v))) => app.open_picker(
            PickKind::Session,
            v.into_iter().map(|s| Item::new(s.id, s.label)).collect(),
        ),
        (Cmd::PickSession, Err(e)) => app.note(&format!("(sessions: {e})")),
        (Cmd::Tree, Ok(HostReply::Sessions(v))) => {
            if v.len() <= 1 {
                app.note("(this session has no parent and no forks)");
            }
            let current = app.footer.session.clone();
            app.open_picker(
                PickKind::Tree,
                v.into_iter().map(|s| Item::new(s.id, s.label)).collect(),
            );
            if let Some(p) = app.picker.as_mut() {
                p.select(&current);
            }
        }
        (Cmd::Tree, Err(e)) => app.note(&format!("(tree: {e})")),
        (Cmd::Fork, Ok(HostReply::Session(id))) => done(app, format!("forked → {id}")),
        (Cmd::Fork, Err(e)) => app.note(&format!("(fork: {e})")),
        (Cmd::New, Ok(HostReply::Session(id))) => done(app, format!("new session {id}")),
        (Cmd::Switch, Ok(HostReply::Session(id))) => done(app, format!("resumed {id}")),
        (Cmd::Status, Ok(HostReply::Lines(lines))) => {
            for l in lines {
                app.note(&l);
            }
        }
        (Cmd::Change, Ok(HostReply::Done)) => done(app, ok_note),
        (Cmd::Attach, Ok(HostReply::Images(v))) => attached(app, v),
        (Cmd::Clear, Ok(HostReply::Images(v))) => {
            app.set_images(v);
            app.note(&format!("({had} queued image(s) dropped)"));
        }
        (Cmd::Images, Ok(HostReply::Images(v))) => {
            app.note(&format!(
                "({} image(s) queued for your next message)",
                v.len()
            ));
            for i in &v {
                app.note(&format!("  {}", i.label()));
            }
            app.set_images(v);
        }
        (Cmd::Copy, Ok(HostReply::Answer(answer))) => copy_answer(app, answer),
        (Cmd::Copy, Err(e)) => app.note(&format!("(/copy: {e})")),
        (Cmd::Export, Ok(HostReply::Path(p))) => app.note(&format!("(transcript written to {p})")),
        (Cmd::Export, Err(e)) => app.note(&format!("(/export: {e})")),
        // The host's own error (jail, size cap, not an image), as chat shows it.
        (Cmd::Attach | Cmd::Clear | Cmd::Images, Err(e)) => app.note(&format!("(/image: {e})")),
        (_, Err(e)) => app.note(&format!("(failed: {e})")),
        // A host answered with the wrong kind of reply: say so, never panic.
        (_, Ok(other)) => app.note(&format!("(failed: unexpected host reply {other:?})")),
    }
}

/// `/copy`: build the OSC 52 sequence for the answer (the loop writes it) and
/// say what was attempted. A terminal cannot confirm OSC 52, so the note never
/// claims the clipboard changed.
fn copy_answer(app: &mut App, answer: Option<String>) {
    let Some(text) = answer.filter(|t| !t.trim().is_empty()) else {
        app.note("(nothing to copy: this session has no assistant answer yet)");
        return;
    };
    if text.len() > clipboard::MAX_COPY_BYTES {
        app.note(&format!(
            "(the last answer is {} KiB, over the {} KiB /copy limit; use /export to write the transcript to a file)",
            text.len().div_ceil(1024),
            clipboard::MAX_COPY_BYTES / 1024
        ));
        return;
    }
    app.note(&format!(
        "(sent {} characters to the terminal clipboard; needs OSC 52 support, tmux needs `set -g set-clipboard on`)",
        text.chars().count()
    ));
    app.osc52 = Some(clipboard::osc52(&text));
}

/// The host accepted an image: show the queue and name the new one.
fn attached(app: &mut App, images: Vec<ImageItem>) {
    if let Some(last) = images.last() {
        app.note(&format!(
            "(image attached to your next message: {})",
            last.label()
        ));
    }
    app.set_images(images);
}

/// A paste that looked like one image reference (#560): attach it, or, when
/// the host refuses, insert the text after all and say why it was not attached.
fn attach_pasted(host: &dyn TuiHost, app: &mut App, spec: String, text: &str) {
    match host.apply(HostCmd::AttachImage(spec)) {
        Ok(HostReply::Images(v)) => attached(app, v),
        Ok(other) => {
            app.paste_text(text);
            app.note(&format!(
                "(pasted as text: unexpected host reply {other:?})"
            ));
        }
        Err(e) => {
            app.paste_text(text);
            app.note(&format!("(pasted as text, not attached: {e})"));
        }
    }
}

/// Backend- and source-agnostic loop (unit-testable with `TestBackend`).
/// `next(timeout)` yields the next terminal event, or `None` on timeout.
pub fn event_loop<B: Backend>(
    term: &mut Terminal<B>,
    host: &dyn TuiHost,
    app: &mut App,
    approvals: &Receiver<Pending>,
    mut next: impl FnMut(Duration) -> io::Result<Option<Event>>,
) -> io::Result<()>
where
    B::Error: std::fmt::Display,
{
    let (tx, rx) = mpsc::channel::<Msg>();
    std::thread::scope(|scope| {
        let mut cancel = CancelToken::default();
        let mut quitting = false;
        let mut dirty = true;
        // One turn on a worker thread; a fresh token per turn.
        let start = |prompt: String| -> CancelToken {
            let cancel = CancelToken::default();
            let (c, tx) = (cancel.clone(), tx.clone());
            scope.spawn(move || {
                let obs: Arc<dyn TurnObserver> = Arc::new(ChanObserver(Mutex::new(tx.clone())));
                let end = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    host.run_turn(&prompt, &c, obs)
                }))
                .unwrap_or_else(|_| TurnEnd::Stopped("turn thread panicked".into()));
                let _ = tx.send(Msg::Done(end));
            });
            cancel
        };
        loop {
            while let Ok(m) = rx.try_recv() {
                if matches!(m, Msg::Done(_)) {
                    // steering the turn never reached a safe point for
                    app.leftover(
                        cancel
                            .interrupts()
                            .drain()
                            .into_iter()
                            .map(|i| i.text)
                            .collect(),
                    );
                }
                app.on_msg(m);
                dirty = true;
            }
            while let Ok(p) = approvals.try_recv() {
                app.on_msg(Msg::Approval(p));
                dirty = true;
            }
            if std::mem::take(&mut app.footer_stale) {
                app.footer = host.footer();
            }
            // A turn that took queued images ended: a message that was never
            // recorded hands them back, so show what the host queues now.
            if !app.running() && std::mem::take(&mut app.images_stale) {
                if let Ok(HostReply::Images(v)) = host.apply(HostCmd::Images) {
                    app.set_images(v);
                }
            }
            if !quitting {
                if let Some(prompt) = app.start_queued() {
                    cancel = start(prompt);
                    dirty = true;
                }
            }
            if quitting && !app.running() {
                commit(term, app)?;
                return Ok(());
            }
            if dirty {
                commit(term, app)?;
                term.draw(|f| app.draw(f)).map_err(io_err::<B>)?;
                dirty = false;
            }
            let Some(ev) = next(Duration::from_millis(30))? else {
                continue;
            };
            dirty = true;
            if let Event::Resize(..) = ev {
                term.autoresize().map_err(io_err::<B>)?;
                continue;
            }
            if let Some(input) = term_input::input_from_event(ev) {
                match app.on_input(input) {
                    Action::Quit if app.running() => {
                        // Unreachable via keys today (quit needs idle), kept safe.
                        cancel.cancel();
                        quitting = true;
                    }
                    Action::Quit => {
                        commit(term, app)?;
                        return Ok(());
                    }
                    Action::Cancel => cancel.cancel(),
                    Action::Submit(prompt) => cancel = start(prompt),
                    Action::Steer(text) => match cancel.interrupts().push(&text, false) {
                        Ok(()) => app.steered(&text),
                        Err(e) => {
                            app.note(&format!("(steer refused: {e})"));
                            app.restore_draft(&text);
                        }
                    },
                    Action::Host(cmd) => {
                        run_host_cmd(host, app, cmd);
                        if let Some(seq) = app.take_osc52() {
                            // Between frames, on the real terminal: never part of a frame.
                            let _ = clipboard::emit(&mut io::stdout(), &seq);
                        }
                    }
                    Action::PasteImage { spec, text } => attach_pasted(host, app, spec, &text),
                    Action::Redraw | Action::None => {}
                }
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::render::inline_spans;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use ratatui::backend::TestBackend;
    use ratatui::style::{Color, Modifier, Style};
    use std::time::Instant;

    fn key(c: KeyCode) -> KeyEvent {
        KeyEvent::new(c, KeyModifiers::NONE)
    }
    fn ctrl(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL)
    }
    fn type_str(app: &mut App, s: &str) {
        for c in s.chars() {
            app.on_key(key(KeyCode::Char(c)));
        }
    }
    fn footer() -> Footer {
        Footer {
            folder: "proj".into(),
            session: "s-1".into(),
            model: Some("gpt".into()),
            effort: Some("high".into()),
            context: "context: 41k / 128k (32%)".into(),
            tokens: "tokens: 10 in / 5 out".into(),
        }
    }

    #[test]
    fn footer_composes_all_fields_and_truncates() {
        let f = footer();
        let line = f.render(200);
        for part in [
            "proj",
            "s-1",
            "gpt",
            "effort: high",
            "41k / 128k (32%)",
            "10 in",
        ] {
            assert!(line.contains(part), "{line}");
        }
        let cut = f.render(20);
        assert_eq!(cut.chars().count(), 20);
        assert!(cut.ends_with('…'));
        assert!(Footer::default().render(80).contains("effort: -"));
    }

    // ---- the UI-neutral input seam (#526): no crossterm types below ----

    fn typed_input(app: &mut App, s: &str) {
        for c in s.chars() {
            app.on_input(Input::new(Key::Char(c)));
        }
    }

    #[test]
    fn a_draft_is_typed_and_submitted_through_inputs_alone() {
        let mut app = App::default();
        typed_input(&mut app, "hello");
        assert_eq!(
            app.on_input(Input::new(Key::Enter)),
            Action::Submit("hello".into())
        );
        assert!(app.running());
    }

    #[test]
    fn modifiers_change_enter_exactly_as_the_key_table_says() {
        // Shift+Enter and Alt+Enter (idle) are newlines; plain Enter submits.
        let mut app = App::default();
        typed_input(&mut app, "a");
        app.on_input(Input::shift(Key::Enter));
        typed_input(&mut app, "b");
        app.on_input(Input::alt(Key::Enter));
        typed_input(&mut app, "c");
        assert_eq!(
            app.on_input(Input::new(Key::Enter)),
            Action::Submit("a\nb\nc".into())
        );
        // Ctrl+J is the newline fallback for terminals without Shift+Enter.
        let mut app = App::default();
        typed_input(&mut app, "x");
        app.on_input(Input::ctrl(Key::Char('j')));
        typed_input(&mut app, "y");
        assert_eq!(
            app.on_input(Input::new(Key::Enter)),
            Action::Submit("x\ny".into())
        );
    }

    #[test]
    fn a_key_the_logic_ignores_still_disarms_ctrl_c() {
        // Ctrl+C once arms; any other key press disarms; Ctrl+C again only
        // arms again. This is why unmapped keys are `Key::Other`, not dropped.
        let mut app = App::default();
        assert_eq!(app.on_input(Input::ctrl(Key::Char('c'))), Action::Redraw);
        assert_eq!(app.on_input(Input::new(Key::Other)), Action::None);
        assert_eq!(app.on_input(Input::ctrl(Key::Char('c'))), Action::Redraw);
        assert_eq!(app.on_input(Input::ctrl(Key::Char('c'))), Action::Quit);
    }

    #[test]
    fn shift_tab_is_its_own_key_not_shift_plus_tab() {
        let mut app = App::new(Footer::default());
        match app.on_input(Input::new(Key::BackTab)) {
            Action::Host(HostCmd::SetEffort(_)) => {}
            other => panic!("BackTab cycles the effort, got {other:?}"),
        }
    }

    // ---- #560: pasted text and image attach, through `Input::Paste` ----

    fn item(name: &str) -> ImageItem {
        ImageItem {
            path: format!("/ws/{name}"),
            mime: "image/png".into(),
            bytes: 1000,
        }
    }

    #[test]
    fn a_paste_is_one_edit_that_never_submits_or_steers() {
        let mut app = App::default();
        typed_input(&mut app, "intro ");
        assert_eq!(
            app.on_input(Input::Paste("one\r\ntwo\rthree\x1b[2J\n".into())),
            Action::Redraw,
            "the newlines in a paste do not submit"
        );
        assert_eq!(app.editor.text(), "intro one\ntwo\nthree[2J\n");
        assert!(!app.running());
        assert_eq!(
            app.on_input(Input::new(Key::Enter)),
            Action::Submit("intro one\ntwo\nthree[2J".into())
        );
        // While a turn runs a paste is still only an edit: Enter steers.
        assert_eq!(
            app.on_input(Input::Paste("more\nlines".into())),
            Action::Redraw
        );
        assert_eq!(app.editor.text(), "more\nlines");
        assert_eq!(
            app.on_input(Input::new(Key::Enter)),
            Action::Steer("more\nlines".into())
        );
        // An empty (or all-control) paste changes nothing.
        assert_eq!(app.on_input(Input::Paste("\x1b\x07".into())), Action::None);
    }

    #[test]
    fn a_paste_never_answers_an_approval_and_disarms_ctrl_c() {
        let mut app = App::default();
        let (tx, rx) = mpsc::channel();
        app.on_msg(Msg::Approval(Pending::new(
            ApprovalRequest::new("write_file a", Risk::Write),
            move |v| {
                let _ = tx.send(v);
            },
        )));
        assert_eq!(app.on_input(Input::Paste("y".into())), Action::None);
        assert!(rx.try_recv().is_err(), "a pasted `y` is not an answer");
        assert_eq!(app.editor.text(), "", "nor typed into the draft");
        app.on_input(Input::new(Key::Char('n')));
        assert_eq!(rx.try_recv(), Ok(Verdict::Deny));

        let mut app = App::default();
        assert_eq!(app.on_input(Input::ctrl(Key::Char('c'))), Action::Redraw);
        app.on_input(Input::Paste("x".into()));
        assert_eq!(
            app.on_input(Input::ctrl(Key::Char('c'))),
            Action::Redraw,
            "a paste disarms a pending Ctrl+C like any key"
        );
    }

    #[test]
    fn a_paste_filters_an_open_picker() {
        let mut app = App::default();
        app.open_picker(
            PickKind::Model,
            vec![Item::new("alpha", "alpha"), Item::new("beta", "beta")],
        );
        assert_eq!(app.on_input(Input::Paste("bet\n".into())), Action::Redraw);
        assert_eq!(
            app.on_input(Input::new(Key::Enter)),
            Action::Host(HostCmd::SetModel("beta".into()))
        );
    }

    #[test]
    fn only_a_lone_image_reference_pasted_while_idle_is_offered_to_the_host() {
        let mut app = App::default();
        assert_eq!(
            app.on_input(Input::Paste(" 'my shot.png'\r\n".into())),
            Action::PasteImage {
                spec: "my shot.png".into(),
                text: " 'my shot.png'\n".into()
            }
        );
        assert_eq!(app.editor.text(), "", "not inserted (yet)");
        for text in ["see shot.png", "a.png\nb.png", "notes.txt"] {
            assert_eq!(app.on_input(Input::Paste(text.into())), Action::Redraw);
        }
        // While a turn runs the host is busy: an image path is plain text.
        let mut app = App::default();
        typed_input(&mut app, "go");
        app.on_input(Input::new(Key::Enter));
        assert_eq!(
            app.on_input(Input::Paste("shot.png".into())),
            Action::Redraw
        );
        assert_eq!(app.editor.text(), "shot.png");
    }

    #[test]
    fn a_pasted_image_is_attached_or_inserted_as_text_with_the_hosts_reason() {
        let host = FakeHost::answering(vec![Ok(HostReply::Images(vec![item("a.png")]))]);
        let mut app = App::default();
        attach_pasted(&host, &mut app, "a.png".into(), "a.png");
        assert_eq!(host.seen(), vec!["AttachImage(\"a.png\")"]);
        assert_eq!(app.editor.text(), "");
        assert_eq!(
            app.image_row(80).as_deref(),
            Some("image: a.png (image/png, 1000 B) · next message")
        );
        assert!(texts(&mut app)
            .iter()
            .any(|n| n == "(image attached to your next message: a.png (image/png, 1000 B))"));

        let host = FakeHost::answering(vec![Err(
            "image: /x.png: outside the workspace jail — refused".into(),
        )]);
        let mut app = App::default();
        attach_pasted(&host, &mut app, "/x.png".into(), "/x.png");
        assert_eq!(app.editor.text(), "/x.png", "inserted after all");
        assert!(app.images().is_empty());
        assert!(texts(&mut app).iter().any(|n| {
            n
            == "(pasted as text, not attached: image: /x.png: outside the workspace jail — refused)"
        }));
    }

    #[test]
    fn slash_image_maps_to_the_three_image_commands() {
        for (draft, want) in [
            ("/image shot.png", HostCmd::AttachImage("shot.png".into())),
            ("/image  clear ", HostCmd::ClearImages),
            ("/image", HostCmd::Images),
        ] {
            let mut app = App::default();
            typed_input(&mut app, draft);
            assert_eq!(
                app.on_input(Input::new(Key::Enter)),
                Action::Host(want),
                "{draft}"
            );
        }
        let (notes, _, _) = host_says(
            HostCmd::AttachImage("/etc/x.png".into()),
            Err("image: /etc/x.png: outside the workspace jail — refused".into()),
        );
        assert_eq!(
            notes,
            vec!["(/image: image: /etc/x.png: outside the workspace jail — refused)"]
        );
        let host = FakeHost::answering(vec![Ok(HostReply::Images(vec![]))]);
        let mut app = App::default();
        app.set_images(vec![item("a.png"), item("b.png")]);
        run_host_cmd(&host, &mut app, HostCmd::ClearImages);
        assert!(app.images().is_empty());
        assert_eq!(texts(&mut app), vec!["(2 queued image(s) dropped)"]);
        let (notes, _, app) =
            host_says(HostCmd::Images, Ok(HostReply::Images(vec![item("a.png")])));
        assert_eq!(
            notes,
            vec![
                "(1 image(s) queued for your next message)",
                "  a.png (image/png, 1000 B)"
            ]
        );
        assert_eq!(app.images().len(), 1);
    }

    fn submit(draft: &str) -> Action {
        let mut app = App::default();
        typed_input(&mut app, draft);
        app.on_input(Input::new(Key::Enter))
    }

    #[test]
    fn copy_and_export_map_to_host_commands() {
        assert_eq!(submit("/copy"), Action::Host(HostCmd::LastAnswer));
        assert_eq!(submit("/export"), Action::Host(HostCmd::Export(None)));
        assert_eq!(
            submit("/export  out/my run.html "),
            Action::Host(HostCmd::Export(Some("out/my run.html".into())))
        );
        // `/copy` takes no argument: refused locally, nothing sent.
        let mut app = App::default();
        typed_input(&mut app, "/copy now");
        assert_eq!(app.on_input(Input::new(Key::Enter)), Action::Redraw);
        assert!(texts(&mut app)[0].contains("usage: /copy"));
        assert!(menu::matching("/co").contains(&"/copy"));
        assert!(menu::matching("/ex").contains(&"/export"));
    }

    #[test]
    fn copy_notes_what_was_attempted_and_queues_only_the_osc52_sequence() {
        let answer = "hi \u{e9}\x1b[2J\x07 ]52; end".to_string();
        let (notes, _, mut app) = host_says(
            HostCmd::LastAnswer,
            Ok(HostReply::Answer(Some(answer.clone()))),
        );
        assert_eq!(notes.len(), 1);
        let n = answer.chars().count();
        assert!(notes[0].contains(&format!("sent {n} characters to the terminal clipboard")));
        assert!(notes[0].contains("OSC 52") && notes[0].contains("set-clipboard on"));
        assert!(
            !notes[0].contains("copied"),
            "never claims success: {}",
            notes[0]
        );
        assert_eq!(app.take_osc52(), Some(clipboard::osc52(&answer)));
        assert_eq!(app.take_osc52(), None, "drains");
    }

    #[test]
    fn copy_with_no_answer_or_too_large_an_answer_sends_nothing() {
        for reply in [None, Some("  \n".to_string())] {
            let (notes, _, mut app) = host_says(HostCmd::LastAnswer, Ok(HostReply::Answer(reply)));
            assert!(notes[0].contains("nothing to copy"), "{notes:?}");
            assert_eq!(app.take_osc52(), None);
        }
        let at_cap = "a".repeat(clipboard::MAX_COPY_BYTES);
        let (_, _, mut app) = host_says(HostCmd::LastAnswer, Ok(HostReply::Answer(Some(at_cap))));
        assert!(app.take_osc52().is_some(), "exactly the cap is allowed");
        let over = "a".repeat(clipboard::MAX_COPY_BYTES + 1);
        let (notes, _, mut app) = host_says(HostCmd::LastAnswer, Ok(HostReply::Answer(Some(over))));
        assert_eq!(app.take_osc52(), None);
        assert!(
            notes[0].contains("/export") && notes[0].contains("64 KiB"),
            "{notes:?}"
        );
        let (notes, _, mut app) = host_says(HostCmd::LastAnswer, Err("boom".into()));
        assert_eq!(notes, vec!["(/copy: boom)"]);
        assert_eq!(app.take_osc52(), None);
    }

    #[test]
    fn export_reports_the_written_path_or_the_hosts_refusal() {
        let (notes, _, _) = host_says(
            HostCmd::Export(None),
            Ok(HostReply::Path("/w/.tole/exports/s-1.html".into())),
        );
        assert_eq!(
            notes,
            vec!["(transcript written to /w/.tole/exports/s-1.html)"]
        );
        let (notes, _, _) = host_says(
            HostCmd::Export(Some("a.html".into())),
            Err("a.html exists; pick another path".into()),
        );
        assert_eq!(notes, vec!["(/export: a.html exists; pick another path)"]);
        // A wrong-kind reply is reported, never a panic.
        let (notes, _, _) = host_says(HostCmd::Export(None), Ok(HostReply::Done));
        assert!(notes[0].contains("unexpected host reply"));
    }

    #[test]
    fn queued_images_leave_with_the_message_and_are_reread_after_the_turn() {
        let mut app = App::default();
        app.set_images(vec![item("a.png"), item("b.png")]);
        assert_eq!(
            app.pending_rows(200),
            vec!["images (2): a.png (image/png, 1000 B), b.png (image/png, 1000 B) · next message"]
        );
        typed_input(&mut app, "look");
        app.on_input(Input::new(Key::Enter));
        assert!(
            app.images().is_empty(),
            "the host took them with the message"
        );
        assert!(app.pending_rows(200).is_empty());
        app.on_msg(Msg::Done(TurnEnd::Stopped("provider failed".into())));
        assert!(
            std::mem::take(&mut app.images_stale),
            "the loop asks the host what is queued now"
        );
        // A turn without images does not ask.
        typed_input(&mut app, "plain");
        app.on_input(Input::new(Key::Enter));
        app.on_msg(Msg::Done(TurnEnd::Final("ok".into())));
        assert!(!app.images_stale);
    }

    #[test]
    fn ctrl_c_twice_quits_but_once_only_arms() {
        let mut app = App::default();
        assert_eq!(app.on_key(ctrl('c')), Action::Redraw);
        assert_eq!(app.on_key(ctrl('c')), Action::Quit);
    }

    #[test]
    fn slash_quit_enter_quits() {
        let mut app = App::default();
        type_str(&mut app, "/quit");
        assert_eq!(app.on_key(key(KeyCode::Enter)), Action::Quit);
    }

    #[test]
    fn enter_submits_shift_enter_and_ctrl_j_insert_newlines() {
        let mut app = App::default();
        type_str(&mut app, "a");
        app.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::SHIFT));
        type_str(&mut app, "b");
        app.on_key(ctrl('j'));
        type_str(&mut app, "c");
        assert_eq!(
            app.on_key(key(KeyCode::Enter)),
            Action::Submit("a\nb\nc".into())
        );
        assert!(app.running());
        let out = app.take_out();
        assert_eq!(out[0].kind, Kind::User);
        assert_eq!(out[0].text, "> a");
    }

    #[test]
    fn enter_is_ignored_while_a_turn_runs_and_blank_does_not_submit() {
        let mut app = App::default();
        assert_eq!(app.on_key(key(KeyCode::Enter)), Action::Redraw);
        type_str(&mut app, "go");
        app.on_key(key(KeyCode::Enter));
        // blank and slash drafts are ignored while running (nothing to steer)
        assert_eq!(app.on_key(key(KeyCode::Enter)), Action::None);
        type_str(&mut app, "/new");
        assert_eq!(app.on_key(key(KeyCode::Enter)), Action::None);
    }

    #[test]
    fn esc_and_ctrl_c_cancel_only_a_running_turn() {
        let mut app = App::default();
        assert_eq!(app.on_key(key(KeyCode::Esc)), Action::None);
        type_str(&mut app, "x");
        app.on_key(key(KeyCode::Enter));
        assert_eq!(app.on_key(key(KeyCode::Esc)), Action::Cancel);
        assert_eq!(app.on_key(ctrl('c')), Action::Cancel);
    }

    #[test]
    fn streamed_text_commits_lines_and_suppresses_final_duplicate() {
        let mut app = App::default();
        type_str(&mut app, "q");
        app.on_key(key(KeyCode::Enter));
        app.take_out();
        app.on_msg(Msg::Text("hel".into()));
        app.on_msg(Msg::Text("lo\nwor".into()));
        app.on_msg(Msg::Tool("▸ read_file a".into()));
        app.on_msg(Msg::Done(TurnEnd::Final("hello\nwor".into())));
        let texts: Vec<_> = app.take_out().into_iter().map(|o| o.text).collect();
        assert_eq!(texts, ["hello", "wor", "▸ read_file a"]);
        assert!(!app.running() && app.footer_stale);
    }

    #[test]
    fn non_streamed_final_text_is_shown_once() {
        let mut app = App::default();
        app.on_msg(Msg::Done(TurnEnd::Final("answer".into())));
        let out = app.take_out();
        assert_eq!(out.len(), 1);
        assert_eq!((out[0].kind, out[0].text.as_str()), (Kind::Text, "answer"));
    }

    #[test]
    fn cancelled_and_stopped_leave_a_note() {
        let mut app = App::default();
        app.on_msg(Msg::Done(TurnEnd::Cancelled));
        app.on_msg(Msg::Done(TurnEnd::Stopped("provider failed".into())));
        let texts: Vec<_> = app.take_out().into_iter().map(|o| o.text).collect();
        assert_eq!(texts, ["(turn cancelled)", "(provider failed)"]);
    }

    fn pending(app: &mut App, destructive: bool) -> Receiver<Verdict> {
        let (tx, rx) = mpsc::channel();
        let risk = if destructive {
            Risk::Destructive
        } else {
            Risk::Write
        };
        app.on_msg(Msg::Approval(Pending::new(
            ApprovalRequest::new("write_file out.txt", risk),
            move |v| {
                let _ = tx.send(v);
            },
        )));
        rx
    }

    #[test]
    fn approval_y_allows_n_denies_other_keys_keep_asking() {
        let mut app = App::default();
        let rx = pending(&mut app, false);
        assert_eq!(app.on_key(key(KeyCode::Char('x'))), Action::None);
        assert!(rx.try_recv().is_err());
        app.on_key(key(KeyCode::Char('y')));
        assert_eq!(rx.recv().unwrap(), Verdict::Allow);
        let rx = pending(&mut app, true);
        app.on_key(key(KeyCode::Char('n')));
        assert_eq!(rx.recv().unwrap(), Verdict::Deny);
    }

    #[test]
    fn approval_esc_denies_and_cancels_and_cancel_drops_to_deny() {
        let mut app = App::default();
        let rx = pending(&mut app, false);
        assert_eq!(app.on_key(key(KeyCode::Esc)), Action::Cancel);
        assert_eq!(rx.recv().unwrap(), Verdict::Deny);
        let rx = pending(&mut app, false);
        app.running = true;
        assert_eq!(app.on_key(ctrl('c')), Action::Cancel);
        // The cancelled approval is dropped unanswered: the drop itself delivers
        // the Deny (fail closed), once, whoever the source is.
        assert_eq!(rx.recv().unwrap(), Verdict::Deny);
        assert!(rx.try_recv().is_err(), "exactly one verdict");
    }

    #[test]
    fn prompter_denies_when_ui_is_gone() {
        let (p, rx) = approval_channel();
        drop(rx);
        assert_eq!(p.ask(ApprovalRequest::new("x", Risk::Write)), Verdict::Deny);
    }

    // ---- approvals as source-agnostic state (#526) ----

    #[test]
    fn an_approval_request_is_one_safe_line_and_carries_its_risk_tier() {
        let r = ApprovalRequest::new("write_file a\x1b[2J\nb", Risk::Write);
        assert_eq!(r.text(), "write_file a\\u{1b}[2J ⏎ b");
        assert_eq!(r.risk(), Risk::Write);
        assert!(!r.is_destructive());
    }

    #[test]
    fn allow_always_is_never_offered_for_a_destructive_call() {
        for (risk, offered) in [
            (Risk::ReadOnly, true),
            (Risk::Write, true),
            (Risk::Destructive, false),
        ] {
            let r = ApprovalRequest::new("x", risk);
            assert_eq!(r.can_allow_always(), offered, "{risk:?}");
            assert_eq!(r.is_destructive(), risk == Risk::Destructive);
        }
    }

    /// A source that is not a turn thread: it posts a request with a reply
    /// closure (a remote adapter would POST the decision there) and never
    /// blocks. Everything the UI does with it is observable on `verdicts`.
    fn remote_source() -> (Prompter, Receiver<Pending>, Arc<Mutex<Vec<Verdict>>>) {
        let (p, rx) = approval_channel();
        (p, rx, Arc::new(Mutex::new(Vec::new())))
    }

    fn post_remote(p: &Prompter, risk: Risk, verdicts: &Arc<Mutex<Vec<Verdict>>>) -> bool {
        let sink = Arc::clone(verdicts);
        p.post(ApprovalRequest::new("remote: deploy", risk), move |v| {
            sink.lock().unwrap().push(v)
        })
    }

    #[test]
    fn a_remote_source_gets_the_users_verdict_exactly_once() {
        for (k, want) in [('y', Verdict::Allow), ('n', Verdict::Deny)] {
            let (p, rx, verdicts) = remote_source();
            assert!(post_remote(&p, Risk::Write, &verdicts));
            let mut app = App::default();
            app.on_msg(Msg::Approval(rx.recv().unwrap()));
            assert!(
                verdicts.lock().unwrap().is_empty(),
                "nothing before the user answers"
            );
            app.on_key(key(KeyCode::Char(k)));
            assert_eq!(*verdicts.lock().unwrap(), [want], "{k}");
            // keys after the answer change nothing
            app.on_key(key(KeyCode::Char('y')));
            assert_eq!(verdicts.lock().unwrap().len(), 1);
        }
    }

    #[test]
    fn esc_denies_and_cancels_for_a_remote_source_too() {
        let (p, rx, verdicts) = remote_source();
        post_remote(&p, Risk::Destructive, &verdicts);
        let mut app = App::default();
        app.on_msg(Msg::Approval(rx.recv().unwrap()));
        assert_eq!(app.on_key(key(KeyCode::Esc)), Action::Cancel);
        assert_eq!(*verdicts.lock().unwrap(), [Verdict::Deny]);
    }

    #[test]
    fn a_destructive_request_is_shown_as_destructive() {
        let (p, rx, verdicts) = remote_source();
        post_remote(&p, Risk::Destructive, &verdicts);
        let mut app = App::default();
        app.on_msg(Msg::Approval(rx.recv().unwrap()));
        let notes = texts(&mut app);
        assert!(
            notes
                .iter()
                .any(|n| n.starts_with("? destructive: remote: deploy")),
            "{notes:?}"
        );
    }

    #[test]
    fn an_approval_that_never_gets_an_answer_is_denied_once_whatever_ended_it() {
        // 1. the turn ends (or is cancelled) while the approval is open
        let (p, rx, verdicts) = remote_source();
        post_remote(&p, Risk::Write, &verdicts);
        let mut app = App::default();
        app.on_msg(Msg::Approval(rx.recv().unwrap()));
        app.on_msg(Msg::Done(TurnEnd::Cancelled));
        assert_eq!(*verdicts.lock().unwrap(), [Verdict::Deny], "turn ended");

        // 2. a newer approval replaces an unanswered one
        let (p, rx, verdicts) = remote_source();
        post_remote(&p, Risk::Write, &verdicts);
        post_remote(&p, Risk::Write, &verdicts);
        let mut app = App::default();
        app.on_msg(Msg::Approval(rx.recv().unwrap()));
        app.on_msg(Msg::Approval(rx.recv().unwrap()));
        assert_eq!(
            *verdicts.lock().unwrap(),
            [Verdict::Deny],
            "the replaced one is denied"
        );

        // 3. the UI goes away with requests still queued
        let (p, rx, verdicts) = remote_source();
        post_remote(&p, Risk::Write, &verdicts);
        post_remote(&p, Risk::Write, &verdicts);
        drop(rx);
        assert_eq!(*verdicts.lock().unwrap(), [Verdict::Deny, Verdict::Deny]);

        // 4. posting to a UI that is already gone: refused, denied at once
        let (p, rx, verdicts) = remote_source();
        drop(rx);
        assert!(!post_remote(&p, Risk::Write, &verdicts));
        assert_eq!(*verdicts.lock().unwrap(), [Verdict::Deny]);
    }

    #[test]
    fn tool_line_shows_first_string_arg_clipped() {
        let v = serde_json::json!({"path": "out.txt", "content": "x"});
        assert!(tool_line("write_file", &v).starts_with("▸ write_file "));
        assert_eq!(tool_line("todo", &serde_json::json!({})), "▸ todo");
        let long = serde_json::json!({"command": "a".repeat(200)});
        assert!(tool_line("run_command", &long).chars().count() < 80);
    }

    #[test]
    fn tool_line_and_finished_carry_no_escape_bytes() {
        let evil = "a\x1b[2J\rb\u{9b}c";
        let line = tool_line("read_file", &serde_json::json!({ "path": evil }));
        assert!(!line.chars().any(|c| c.is_control()), "{line:?}");
        let (tx, rx) = mpsc::channel();
        ChanObserver(Mutex::new(tx)).tool_finished("t", true, evil);
        let Ok(Msg::ToolResult { ok, preview }) = rx.try_recv() else {
            panic!("no tool msg")
        };
        assert!(ok);
        let mut app = App::default();
        app.on_msg(Msg::ToolResult { ok, preview });
        for o in app.take_out() {
            assert!(!o.text.chars().any(|c| c.is_control()), "{:?}", o.text);
        }
    }

    #[test]
    fn stream_note_approval_and_footer_are_sanitized() {
        let mut app = App::default();
        app.on_msg(Msg::Text("ok\x1b[2J\rx\nnext".into()));
        app.on_msg(Msg::Done(TurnEnd::Stopped("bad\x1b]0;t\x07".into())));
        app.on_msg(Msg::Approval(Pending::new(
            ApprovalRequest::new("w\x1b[2K", Risk::Write),
            |_| {},
        )));
        for o in app.take_out() {
            assert!(!o.text.chars().any(|c| c.is_control()), "{:?}", o.text);
        }
        let f = Footer {
            model: Some("m\x1b[2J".into()),
            ..footer()
        };
        assert!(!f.render(200).chars().any(|c| c.is_control()));
        let (p, rx) = approval_channel();
        std::thread::spawn(move || p.ask(ApprovalRequest::new("a\x1b\nb", Risk::Write)));
        assert_eq!(rx.recv().unwrap().request().text(), "a\\u{1b} ⏎ b");
    }

    #[test]
    fn wrap_splits_by_width() {
        assert_eq!(wrap("abcde", 2), ["ab", "cd", "e"]);
        assert_eq!(wrap("", 4), [""]);
    }

    struct Host;
    impl TuiHost for Host {
        fn footer(&self) -> Footer {
            footer()
        }
        fn run_turn(&self, p: &str, _c: &CancelToken, o: Arc<dyn TurnObserver>) -> TurnEnd {
            o.text_delta("echo: ");
            o.text_delta(p);
            o.text_delta("\n");
            TurnEnd::Final(format!("echo: {p}"))
        }
    }

    #[test]
    fn loop_runs_a_turn_streams_it_and_quits() {
        let mut term = Terminal::with_options(
            TestBackend::new(60, 12),
            TerminalOptions {
                viewport: Viewport::Inline(VIEWPORT_HEIGHT),
            },
        )
        .unwrap();
        let (_p, rx) = approval_channel();
        let mut script: Vec<Option<Event>> = "hi"
            .chars()
            .map(|c| Some(Event::Key(key(KeyCode::Char(c)))))
            .collect();
        script.push(Some(Event::Key(key(KeyCode::Enter))));
        // idle ticks while the turn thread finishes, then quit
        script.extend(std::iter::repeat_n(None, 30));
        script.push(Some(Event::Key(ctrl('c'))));
        script.push(Some(Event::Key(ctrl('c'))));
        let mut it = script.into_iter();
        event_loop(&mut term, &Host, &mut App::new(footer()), &rx, |_| {
            std::thread::sleep(Duration::from_millis(5));
            Ok(it.next().unwrap_or(None))
        })
        .unwrap();
        let text = format!("{}", term.backend());
        assert!(text.contains("echo: hi"), "{text}");
        assert!(text.contains("proj · s-1"), "{text}");
    }

    fn shift(c: KeyCode) -> KeyEvent {
        KeyEvent::new(c, KeyModifiers::SHIFT)
    }
    fn typed(app: &mut App, s: &str) -> Action {
        type_str(app, s);
        app.on_key(key(KeyCode::Enter))
    }
    fn notes(app: &mut App) -> String {
        app.take_out()
            .into_iter()
            .map(|o| o.text)
            .collect::<Vec<_>>()
            .join("|")
    }

    #[test]
    fn shift_tab_cycles_effort_and_shift_enter_still_newlines() {
        let mut app = App::new(Footer::default());
        let mut seq = Vec::new();
        for want in ["minimal", "low", "medium", "high"] {
            let Action::Host(HostCmd::SetEffort(Some(l))) = app.on_key(shift(KeyCode::BackTab))
            else {
                panic!("no cycle")
            };
            assert_eq!(l, want);
            app.footer.effort = Some(l.clone());
            seq.push(l);
        }
        // after `high` the cycle goes to unset, then wraps to minimal
        assert_eq!(
            app.on_key(shift(KeyCode::BackTab)),
            Action::Host(HostCmd::SetEffort(None))
        );
        app.footer.effort = None;
        assert_eq!(
            app.on_key(shift(KeyCode::BackTab)),
            Action::Host(HostCmd::SetEffort(Some("minimal".into())))
        );
        // Shift+Enter is untouched: newline, never a submit or a cycle
        type_str(&mut app, "a");
        assert_eq!(app.on_key(shift(KeyCode::Enter)), Action::Redraw);
        type_str(&mut app, "b");
        assert_eq!(app.editor.text(), "a\nb");
    }

    #[test]
    fn command_menu_filters_as_you_type_and_tab_completes() {
        let mut app = App::default();
        assert!(app.menu().is_empty(), "no menu without a slash");
        type_str(&mut app, "/");
        assert_eq!(app.menu().len(), menu::COMMANDS.len());
        type_str(&mut app, "com");
        assert_eq!(app.menu(), ["/compact"]);
        app.on_key(key(KeyCode::Tab));
        assert_eq!(app.editor.text(), "/compact ");
        assert!(app.menu().is_empty(), "arguments typed: menu closes");
        let mut app = App::default();
        type_str(&mut app, "/");
        app.on_key(key(KeyCode::Down));
        app.on_key(key(KeyCode::Tab));
        assert_eq!(app.editor.text(), "/thinking ", "Down moves the selection");
    }

    #[test]
    fn unknown_commands_are_refused_with_a_hint() {
        let mut app = App::default();
        assert_eq!(typed(&mut app, "/frobnicate"), Action::Redraw);
        assert!(!app.running());
        let n = notes(&mut app);
        assert!(n.contains("unknown command /frobnicate"), "{n}");
        assert_eq!(typed(&mut app, "/mod"), Action::Redraw);
        let n = notes(&mut app);
        assert!(n.contains("did you mean /model"), "{n}");
        assert_eq!(typed(&mut app, "/x\x1b[2J"), Action::Redraw);
        assert!(!notes(&mut app).chars().any(|c| c == '\x1b'));
    }

    #[test]
    fn unknown_hint_never_panics_on_empty_or_multibyte_input() {
        assert!(menu::unknown("").contains("unknown command"));
        assert!(menu::unknown("日本語").contains("unknown command"));
        assert!(menu::unknown("/日本語").contains("unknown command"));
    }

    #[test]
    fn commands_map_to_host_actions() {
        let host = |c| Action::Host(c);
        let mut app = App::default();
        assert_eq!(typed(&mut app, "/model"), host(HostCmd::PickModel));
        assert_eq!(
            typed(&mut app, "/model gpt-x"),
            host(HostCmd::SetModel("gpt-x".into()))
        );
        assert_eq!(
            typed(&mut app, "/thinking low"),
            host(HostCmd::SetEffort(Some("low".into())))
        );
        assert_eq!(
            typed(&mut app, "/thinking unset"),
            host(HostCmd::SetEffort(None))
        );
        assert_eq!(
            typed(&mut app, "/thinking"),
            host(HostCmd::SetEffort(Some("minimal".into())))
        );
        assert_eq!(typed(&mut app, "/thinking loud"), Action::Redraw);
        assert!(notes(&mut app).contains("minimal | low | medium | high | unset"));
        assert_eq!(typed(&mut app, "/compact"), host(HostCmd::Compact(None)));
        assert_eq!(
            typed(&mut app, "/compact keep the api"),
            host(HostCmd::Compact(Some("keep the api".into())))
        );
        assert_eq!(typed(&mut app, "/new"), host(HostCmd::New));
        assert_eq!(typed(&mut app, "/resume"), host(HostCmd::PickSession));
        assert_eq!(typed(&mut app, "/session"), host(HostCmd::Status));
        assert_eq!(
            typed(&mut app, "/name  my run "),
            host(HostCmd::Name("my run".into()))
        );
        assert_eq!(typed(&mut app, "/name"), Action::Redraw);
        assert!(notes(&mut app).contains("usage: /name"));
        assert_eq!(
            app.on_key(KeyEvent::new(KeyCode::Char('l'), KeyModifiers::CONTROL)),
            host(HostCmd::PickModel)
        );
    }

    #[test]
    fn picker_filters_selects_and_escapes_and_sanitizes() {
        let mut app = App::default();
        app.open_picker(
            PickKind::Model,
            vec![
                Item::new("gpt-4o", "gpt-4o"),
                Item::new("claude\x1b[2J", "claude\x1b[2J"),
                Item::new("gpt-mini", "gpt-mini"),
            ],
        );
        for line in app.picker_lines() {
            assert!(!line.chars().any(|c| c.is_control()), "{line:?}");
        }
        type_str(&mut app, "mini");
        assert_eq!(app.picker_lines().len(), 2, "header + one match");
        assert_eq!(
            app.on_key(key(KeyCode::Enter)),
            Action::Host(HostCmd::SetModel("gpt-mini".into()))
        );
        assert!(app.picker_lines().is_empty(), "closed after choosing");
        app.open_picker(
            PickKind::Model,
            vec![Item::new("a", "a"), Item::new("b", "b")],
        );
        app.on_key(key(KeyCode::Down));
        assert_eq!(
            app.on_key(key(KeyCode::Enter)),
            Action::Host(HostCmd::SetModel("b".into()))
        );
        app.open_picker(PickKind::Session, vec![Item::new("s-1", "first")]);
        assert_eq!(
            app.on_key(key(KeyCode::Enter)),
            Action::Host(HostCmd::Switch("s-1".into()))
        );
        app.open_picker(PickKind::Model, vec![Item::new("a", "a")]);
        assert_eq!(app.on_key(key(KeyCode::Esc)), Action::Redraw);
        assert!(app.picker_lines().is_empty());
        assert_eq!(app.editor.text(), "", "picker keys never reach the editor");
    }

    #[test]
    fn ctrl_p_cycles_recent_models_and_hotkeys_wait_for_idle() {
        let mut app = App::new(Footer {
            model: Some("m1".into()),
            ..Footer::default()
        });
        assert_eq!(
            app.on_key(ctrl('p')),
            Action::Redraw,
            "nothing to cycle yet"
        );
        assert!(notes(&mut app).contains("no other recent model"));
        app.note_model_chosen("m2");
        app.note_model_chosen("m3");
        let mut got = Vec::new();
        for _ in 0..3 {
            let Action::Host(HostCmd::SetModel(m)) = app.on_key(ctrl('p')) else {
                panic!("no cycle")
            };
            got.push(m);
        }
        assert_eq!(got, ["m2", "m1", "m3"]);
        type_str(&mut app, "x");
        app.on_key(key(KeyCode::Enter));
        assert!(app.running());
        for k in [ctrl('l'), ctrl('p'), shift(KeyCode::BackTab)] {
            assert_eq!(app.on_key(k), Action::None);
        }
    }

    struct Switchable(Mutex<Option<String>>);
    impl TuiHost for Switchable {
        fn footer(&self) -> Footer {
            Footer {
                effort: self.0.lock().unwrap().clone(),
                ..footer()
            }
        }
        fn run_turn(&self, _: &str, _: &CancelToken, _: Arc<dyn TurnObserver>) -> TurnEnd {
            TurnEnd::Final(String::new())
        }
        fn apply(&self, cmd: HostCmd) -> Result<HostReply, String> {
            match cmd {
                HostCmd::SetEffort(level) => {
                    *self.0.lock().unwrap() = level;
                    Ok(HostReply::Done)
                }
                _ => unsupported(),
            }
        }
    }

    // ---- the host interface a remote adapter implements (#526) ----

    /// A host with only the three methods the trait asks for: it records
    /// every command it is given and answers with scripted replies. Nothing in
    /// it knows about sessions, storage or a terminal.
    struct FakeHost {
        seen: Mutex<Vec<String>>,
        replies: Mutex<std::collections::VecDeque<Result<HostReply, String>>>,
    }

    impl FakeHost {
        fn answering(replies: Vec<Result<HostReply, String>>) -> Self {
            FakeHost {
                seen: Mutex::new(Vec::new()),
                replies: Mutex::new(replies.into()),
            }
        }
        fn seen(&self) -> Vec<String> {
            self.seen.lock().unwrap().clone()
        }
    }

    impl TuiHost for FakeHost {
        fn footer(&self) -> Footer {
            footer()
        }
        fn run_turn(&self, _: &str, _: &CancelToken, _: Arc<dyn TurnObserver>) -> TurnEnd {
            TurnEnd::Final(String::new())
        }
        fn apply(&self, cmd: HostCmd) -> Result<HostReply, String> {
            self.seen.lock().unwrap().push(format!("{cmd:?}"));
            self.replies
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or_else(|| Err("no scripted reply".into()))
        }
    }

    /// Run `cmd` against a host that answers `reply`; the UI's notes and
    /// whether the footer was marked stale.
    fn host_says(cmd: HostCmd, reply: Result<HostReply, String>) -> (Vec<String>, bool, App) {
        let host = FakeHost::answering(vec![reply]);
        let mut app = App {
            footer_stale: false,
            ..App::default()
        };
        run_host_cmd(&host, &mut app, cmd);
        assert_eq!(host.seen().len(), 1, "exactly one host call per command");
        let stale = app.footer_stale;
        (texts(&mut app), stale, app)
    }

    #[test]
    fn a_change_is_one_apply_call_and_the_wording_is_the_uis() {
        let done = || Ok(HostReply::Done);
        let cases: Vec<(HostCmd, &str)> = vec![
            (HostCmd::SetModel("m1".into()), "(model → m1)"),
            (HostCmd::SetEffort(Some("low".into())), "(thinking → low)"),
            (HostCmd::SetEffort(None), "(thinking → unset)"),
            (
                HostCmd::Compact(None),
                "(compaction runs before your next prompt)",
            ),
            (HostCmd::Name("nightly".into()), "(named nightly)"),
        ];
        for (cmd, want) in cases {
            let (notes, stale, _) = host_says(cmd, done());
            assert!(notes.iter().any(|n| n.contains(want)), "{want}: {notes:?}");
            assert!(stale, "a change refreshes the footer: {want}");
        }
        let (notes, _, _) = host_says(HostCmd::Compact(Some("keep tests".into())), done());
        assert!(
            notes
                .iter()
                .any(|n| n.contains("instructions are not supported yet")),
            "{notes:?}"
        );
    }

    #[test]
    fn new_and_switch_report_the_session_the_host_names() {
        let (notes, stale, _) = host_says(HostCmd::New, Ok(HostReply::Session("s-9".into())));
        assert!(
            notes.iter().any(|n| n.contains("new session s-9")),
            "{notes:?}"
        );
        assert!(stale);
        let (notes, _, _) = host_says(
            HostCmd::Switch("s-1".into()),
            Ok(HostReply::Session("s-1".into())),
        );
        assert!(notes.iter().any(|n| n.contains("resumed s-1")), "{notes:?}");
    }

    #[test]
    fn fork_and_tree_commands_map_to_host_actions() {
        let host = |c| Action::Host(c);
        let mut app = App::default();
        assert_eq!(typed(&mut app, "/fork"), host(HostCmd::Fork(None)));
        assert_eq!(
            typed(&mut app, "/fork  e-12 "),
            host(HostCmd::Fork(Some("e-12".into())))
        );
        assert_eq!(typed(&mut app, "/tree"), host(HostCmd::PickTree));
        // the command menu lists both
        type_str(&mut app, "/f");
        assert!(app.menu().contains(&"/fork"));
        app.on_key(key(KeyCode::Esc));
        // while a turn runs a command is ignored, like /new and /resume
        let mut app = running_app();
        for c in ["/fork", "/tree", "/new", "/resume"] {
            type_str(&mut app, c);
            assert_eq!(app.on_key(key(KeyCode::Enter)), Action::None, "{c}");
        }
    }

    #[test]
    fn fork_reports_the_new_session_and_refusals_with_the_hosts_error() {
        let (notes, stale, _) =
            host_says(HostCmd::Fork(None), Ok(HostReply::Session("s-fork".into())));
        assert!(
            notes.iter().any(|n| n.contains("forked → s-fork")),
            "{notes:?}"
        );
        assert!(stale, "the footer is re-read after the switch");
        let (notes, stale, _) = host_says(
            HostCmd::Fork(Some("e-9".into())),
            Err("forking s-1: entry e-9 is not a settled point".into()),
        );
        assert!(
            notes
                .iter()
                .any(|n| n.contains("(fork: forking s-1: entry e-9 is not a settled point)")),
            "{notes:?}"
        );
        assert!(!stale);
    }

    fn tree_rows() -> Vec<SessionItem> {
        vec![
            SessionItem {
                id: "s-1".into(),
                label: "s-1  2 turns".into(),
            },
            SessionItem {
                id: "s-2".into(),
                label: "└─ s-2 (@e-4)  3 turns  (current)".into(),
            },
            SessionItem {
                id: "s-3".into(),
                label: "   └─ s-3 (@e-6)  4 turns".into(),
            },
        ]
    }

    #[test]
    fn tree_picker_shows_the_family_preselects_the_current_and_switches() {
        let mut app = App::default();
        app.footer.session = "s-2".into();
        let host = FakeHost::answering(vec![Ok(HostReply::Sessions(tree_rows()))]);
        run_host_cmd(&host, &mut app, HostCmd::PickTree);
        assert_eq!(host.seen(), vec!["PickTree"]);
        let shown = app.picker_lines();
        assert!(shown[0].starts_with("tree> "), "{shown:?}");
        assert!(
            shown.iter().any(|l| l.contains("▸ └─ s-2 (@e-4)")),
            "{shown:?}"
        );
        assert!(
            shown.iter().any(|l| l.contains("   └─ s-3")),
            "indented: {shown:?}"
        );
        assert!(shown.iter().any(|l| l.ends_with("(current)")), "{shown:?}");
        // a family of three: no "alone" note
        assert!(!texts(&mut app).iter().any(|n| n.contains("no parent")));
        // Enter on the preselected (current) row switches nowhere
        assert_eq!(app.on_key(key(KeyCode::Enter)), Action::Redraw);
        assert!(texts(&mut app).iter().any(|n| n.contains("already in s-2")));
        // another row switches to exactly that id
        run_host_cmd(
            &FakeHost::answering(vec![Ok(HostReply::Sessions(tree_rows()))]),
            &mut app,
            HostCmd::PickTree,
        );
        app.on_key(key(KeyCode::Down));
        assert_eq!(
            app.on_key(key(KeyCode::Enter)),
            Action::Host(HostCmd::Switch("s-3".into()))
        );
        // and the first row goes to the root
        run_host_cmd(
            &FakeHost::answering(vec![Ok(HostReply::Sessions(tree_rows()))]),
            &mut app,
            HostCmd::PickTree,
        );
        app.on_key(key(KeyCode::Up));
        assert_eq!(
            app.on_key(key(KeyCode::Enter)),
            Action::Host(HostCmd::Switch("s-1".into()))
        );
    }

    #[test]
    fn a_lone_session_in_tree_is_shown_with_a_note() {
        let one = vec![SessionItem {
            id: "s-1".into(),
            label: "s-1  1 turns  (current)".into(),
        }];
        let (notes, _, app) = host_says(HostCmd::PickTree, Ok(HostReply::Sessions(one)));
        assert!(
            notes.iter().any(|n| n.contains("no parent and no forks")),
            "{notes:?}"
        );
        assert!(app.picker_lines().iter().any(|l| l.contains("s-1")));
        let (notes, _, _) = host_says(HostCmd::PickTree, Err("boom".into()));
        assert!(
            notes.iter().any(|n| n.contains("(tree: boom)")),
            "{notes:?}"
        );
    }

    #[test]
    fn pickers_open_from_the_hosts_data_and_empty_or_failed_ones_say_so() {
        let ids = vec!["gpt-a".to_string(), "gpt-b".to_string()];
        let (_, stale, app) = host_says(HostCmd::PickModel, Ok(HostReply::Models(ids)));
        assert!(!stale, "a picker changes nothing");
        let shown = app.picker_lines().join("\n");
        assert!(
            shown.contains("gpt-a") && shown.contains("gpt-b"),
            "{shown}"
        );
        let (notes, _, app) = host_says(HostCmd::PickModel, Ok(HostReply::Models(vec![])));
        assert!(app.picker_lines().is_empty());
        assert!(
            notes.iter().any(|n| n.contains("lists no models")),
            "{notes:?}"
        );
        let (notes, _, _) = host_says(HostCmd::PickModel, Err("offline".into()));
        assert!(notes
            .iter()
            .any(|n| n.contains("(models: offline; use /model <id>)")));

        let rows = vec![SessionItem {
            id: "s-2".into(),
            label: "fix parser".into(),
        }];
        let (_, _, app) = host_says(HostCmd::PickSession, Ok(HostReply::Sessions(rows)));
        let shown = app.picker_lines().join("\n");
        assert!(shown.contains("fix parser"), "{shown}");
        let (notes, _, _) = host_says(HostCmd::PickSession, Ok(HostReply::Sessions(vec![])));
        assert!(
            notes.iter().any(|n| n.contains("no other sessions")),
            "{notes:?}"
        );
        let (notes, _, _) = host_says(HostCmd::PickSession, Err("no index".into()));
        assert!(notes.iter().any(|n| n.contains("(sessions: no index)")));
    }

    #[test]
    fn status_lines_are_shown_as_given() {
        let lines = vec!["session: s-1".to_string(), "model: m".to_string()];
        let (notes, stale, _) = host_says(HostCmd::Status, Ok(HostReply::Lines(lines)));
        assert!(notes.contains(&"session: s-1".to_string()), "{notes:?}");
        assert!(notes.contains(&"model: m".to_string()));
        assert!(!stale);
    }

    #[test]
    fn a_failing_host_is_reported_and_leaves_the_footer_alone() {
        let (notes, stale, _) = host_says(HostCmd::SetModel("x".into()), Err("nope".into()));
        assert!(
            notes.iter().any(|n| n.contains("(failed: nope)")),
            "{notes:?}"
        );
        assert!(!stale, "nothing changed, nothing to refresh");
    }

    #[test]
    fn a_reply_of_the_wrong_kind_is_reported_not_a_panic() {
        // A remote adapter that answers a model change with a model list.
        let (notes, stale, _) = host_says(
            HostCmd::SetModel("x".into()),
            Ok(HostReply::Models(vec!["a".into()])),
        );
        assert!(
            notes.iter().any(|n| n.contains("unexpected host reply")),
            "{notes:?}"
        );
        assert!(!stale);
    }

    #[test]
    fn a_host_that_implements_nothing_supports_status_and_refuses_the_rest() {
        // `Host` (the echo host above) overrides only footer and run_turn.
        let mut app = App::default();
        run_host_cmd(&Host, &mut app, HostCmd::Status);
        assert!(texts(&mut app).is_empty(), "no status lines, no error");
        for cmd in [
            HostCmd::PickModel,
            HostCmd::SetModel("x".into()),
            HostCmd::New,
            HostCmd::PickSession,
        ] {
            run_host_cmd(&Host, &mut app, cmd);
        }
        let notes = texts(&mut app);
        assert_eq!(notes.len(), 4, "{notes:?}");
        assert!(
            notes
                .iter()
                .all(|n| n.contains("not supported by this host")),
            "{notes:?}"
        );
    }

    /// #560 through the loop: a terminal paste event becomes one host
    /// attach call and the image row; a multi-line paste is only drawn.
    #[test]
    fn the_loop_attaches_a_pasted_image_and_draws_a_pasted_block() {
        let host = FakeHost::answering(vec![Ok(HostReply::Images(vec![item("shot.png")]))]);
        let mut term = Terminal::with_options(
            TestBackend::new(100, 14),
            TerminalOptions {
                viewport: Viewport::Inline(VIEWPORT_HEIGHT),
            },
        )
        .unwrap();
        let (_p, rx) = approval_channel();
        let mut script = vec![
            Some(Event::Paste("shot.png".into())),
            Some(Event::Paste("first\r\nsecond".into())),
        ];
        script.extend(std::iter::repeat_n(None, 3));
        let mut it = script.into_iter();
        let mut app = App::new(footer());
        event_loop(&mut term, &host, &mut app, &rx, |_| {
            Ok(it.next().unwrap_or(Some(Event::Key(ctrl('c')))))
        })
        .unwrap();
        assert_eq!(host.seen(), vec!["AttachImage(\"shot.png\")"]);
        assert_eq!(app.editor.text(), "first\nsecond");
        let text = format!("{}", term.backend());
        assert!(
            text.contains("image: shot.png (image/png, 1000 B) · next message"),
            "{text}"
        );
        assert!(
            text.contains("> first") && text.contains("  second"),
            "{text}"
        );
    }

    #[test]
    fn a_switch_refreshes_the_footer_on_the_next_loop_turn() {
        let host = Switchable(Mutex::new(None));
        let mut term = Terminal::with_options(
            TestBackend::new(100, 14),
            TerminalOptions {
                viewport: Viewport::Inline(VIEWPORT_HEIGHT),
            },
        )
        .unwrap();
        let (_p, rx) = approval_channel();
        let mut script = vec![Some(Event::Key(shift(KeyCode::BackTab)))];
        script.extend(std::iter::repeat_n(None, 5));
        script.push(Some(Event::Key(ctrl('c'))));
        script.push(Some(Event::Key(ctrl('c'))));
        let mut it = script.into_iter();
        let mut app = App::new(host.footer());
        event_loop(&mut term, &host, &mut app, &rx, |_| {
            Ok(it.next().unwrap_or(None))
        })
        .unwrap();
        assert_eq!(host.0.lock().unwrap().as_deref(), Some("minimal"));
        assert_eq!(app.footer.effort.as_deref(), Some("minimal"));
        let text = format!("{}", term.backend());
        assert!(text.contains("effort: minimal"), "{text}");
        assert!(text.contains("thinking: minimal"), "editor border: {text}");
    }

    // ---- #462: steering, follow-up queue, toggles, @file, markdown/diff ----

    fn alt(c: KeyCode) -> KeyEvent {
        KeyEvent::new(c, KeyModifiers::ALT)
    }
    fn running_app() -> App {
        let mut app = App::default();
        typed(&mut app, "go");
        app.take_out();
        assert!(app.running());
        app
    }
    fn texts(app: &mut App) -> Vec<String> {
        app.take_out().into_iter().map(|o| o.text).collect()
    }

    #[test]
    fn enter_while_running_steers_and_idle_enter_still_submits() {
        let mut app = running_app();
        type_str(&mut app, "use rust");
        assert_eq!(
            app.on_key(key(KeyCode::Enter)),
            Action::Steer("use rust".into())
        );
        assert_eq!(app.editor.text(), "");
        app.steered("use rust");
        assert!(texts(&mut app)[0].contains("use rust"));
        let mut idle = App::default();
        assert_eq!(typed(&mut idle, "hi"), Action::Submit("hi".into()));
    }

    #[test]
    fn a_refused_steer_goes_back_to_the_editor() {
        let mut app = running_app();
        app.restore_draft("lost?");
        assert_eq!(app.editor.text(), "lost?");
    }

    #[test]
    fn alt_enter_queues_while_running_but_is_a_newline_when_idle() {
        let mut idle = App::default();
        type_str(&mut idle, "a");
        idle.on_key(alt(KeyCode::Enter));
        type_str(&mut idle, "b");
        assert_eq!(idle.editor.text(), "a\nb");

        let mut app = running_app();
        type_str(&mut app, "one");
        assert_eq!(app.on_key(alt(KeyCode::Enter)), Action::Redraw);
        type_str(&mut app, "two");
        app.on_key(alt(KeyCode::Enter));
        assert_eq!(app.editor.text(), "");
        assert_eq!(app.queue_rows(80), ["queued: one", "queued: two"]);
        // blank draft queues nothing
        app.on_key(alt(KeyCode::Enter));
        assert_eq!(app.queue_rows(80).len(), 2);
    }

    #[test]
    fn queue_rows_are_sanitized_clipped_and_capped() {
        let mut app = running_app();
        for m in ["a\x1b[2J\rb", "c", "d", "e", "f"] {
            type_str(&mut app, m);
            app.on_key(alt(KeyCode::Enter));
        }
        let rows = app.queue_rows(80);
        assert_eq!(rows.len(), 3);
        assert!(rows[2].contains("+3 more"), "{rows:?}");
        assert!(rows.iter().all(|r| !r.chars().any(|c| c.is_control())));
        assert_eq!(rows[0], "queued: a\\u{1b}[2J\\u{d}b");
        assert!(app.queue_rows(6)[0].chars().count() <= 6);
    }

    #[test]
    fn alt_up_pops_the_last_queued_message_into_the_editor() {
        let mut app = running_app();
        for m in ["one", "two"] {
            type_str(&mut app, m);
            app.on_key(alt(KeyCode::Enter));
        }
        app.on_key(alt(KeyCode::Up));
        assert_eq!(app.editor.text(), "two");
        assert_eq!(app.queue_rows(80), ["queued: one"]);
        // with a draft present the popped message goes before it
        type_str(&mut app, "!");
        app.on_key(alt(KeyCode::Up));
        assert_eq!(app.editor.text(), "one\ntwo!");
        assert_eq!(app.on_key(alt(KeyCode::Up)), Action::None);
    }

    #[test]
    fn cancel_returns_every_queued_message_to_the_editor() {
        for cancel in [key(KeyCode::Esc), ctrl('c')] {
            let mut app = running_app();
            for m in ["one", "two"] {
                type_str(&mut app, m);
                app.on_key(alt(KeyCode::Enter));
            }
            assert_eq!(app.on_key(cancel), Action::Cancel);
            assert_eq!(app.editor.text(), "one\ntwo");
            assert!(app.queue_rows(80).is_empty());
        }
    }

    #[test]
    fn queue_head_auto_starts_after_a_final_one_at_a_time() {
        let mut app = running_app();
        for m in ["one", "two"] {
            type_str(&mut app, m);
            app.on_key(alt(KeyCode::Enter));
        }
        assert_eq!(app.start_queued(), None, "not while running");
        app.on_msg(Msg::Done(TurnEnd::Final("ok".into())));
        app.take_out();
        assert_eq!(app.start_queued(), Some("one".into()));
        assert!(app.running());
        assert_eq!(texts(&mut app), ["> one"]);
        assert_eq!(app.queue_rows(80), ["queued: two"]);
        assert_eq!(app.start_queued(), None, "one at a time");
        app.on_msg(Msg::Done(TurnEnd::Final("ok".into())));
        assert_eq!(app.start_queued(), Some("two".into()));
    }

    #[test]
    fn a_stopped_turn_returns_the_queue_instead_of_running_it() {
        let mut app = running_app();
        type_str(&mut app, "one");
        app.on_key(alt(KeyCode::Enter));
        app.on_msg(Msg::Done(TurnEnd::Stopped("provider failed".into())));
        assert_eq!(app.start_queued(), None);
        assert_eq!(app.editor.text(), "one");
    }

    #[test]
    fn unconsumed_steering_is_not_lost_when_the_turn_settles() {
        let mut app = running_app();
        app.leftover(vec!["steer-a".into()]);
        app.on_msg(Msg::Done(TurnEnd::Final("ok".into())));
        assert_eq!(app.start_queued(), Some("steer-a".into()));
        let mut app = running_app();
        app.leftover(vec!["steer-b".into()]);
        app.on_msg(Msg::Done(TurnEnd::Cancelled));
        assert_eq!(app.editor.text(), "steer-b");
    }

    #[test]
    fn ctrl_t_hides_and_shows_reasoning_for_new_output() {
        let mut app = App::default();
        app.on_msg(Msg::Reasoning("shown\n".into()));
        app.on_key(ctrl('t'));
        app.on_msg(Msg::Reasoning("secret\n".into()));
        app.on_msg(Msg::Text("ans\n".into()));
        app.on_key(ctrl('t'));
        app.on_msg(Msg::Reasoning("again\n".into()));
        let t = texts(&mut app).join("|");
        assert!(t.contains("shown") && t.contains("again"), "{t}");
        assert!(!t.contains("secret"), "{t}");
        assert!(
            t.contains("reasoning hidden") && t.contains("reasoning shown"),
            "{t}"
        );
    }

    #[test]
    fn tool_output_is_collapsed_with_a_hint_and_ctrl_o_expands_the_last_block() {
        let mut app = App::default();
        app.on_msg(Msg::ToolResult {
            ok: true,
            preview: "first\nsecond\nthird".into(),
        });
        let t = texts(&mut app);
        assert_eq!(t[0], "  ✓ first");
        assert!(
            t[1].contains("[+2 lines") && t[1].contains("Ctrl+O"),
            "{t:?}"
        );
        assert_eq!(t.len(), 2);
        app.on_key(ctrl('o'));
        let t = texts(&mut app);
        assert!(t.iter().any(|l| l.ends_with("second")), "{t:?}");
        assert!(t.iter().any(|l| l.ends_with("third")), "{t:?}");
        // expanded: the next block prints in full, no hint
        app.on_msg(Msg::ToolResult {
            ok: true,
            preview: "a\nb".into(),
        });
        let t = texts(&mut app);
        assert!(t.iter().any(|l| l.ends_with('b')) && !t.iter().any(|l| l.contains("[+")));
        app.on_key(ctrl('o'));
        app.take_out();
        app.on_msg(Msg::ToolResult {
            ok: true,
            preview: "a\nb".into(),
        });
        assert!(texts(&mut app).iter().any(|l| l.contains("[+1 line")));
    }

    #[test]
    fn at_panel_filters_files_and_tab_inserts_the_path() {
        let mut app = App::default();
        app.set_files(vec![
            "src/main.rs".into(),
            "src/lib.rs".into(),
            "README.md".into(),
        ]);
        type_str(&mut app, "see @li");
        assert_eq!(app.at_matches(), ["src/lib.rs"]);
        app.on_key(key(KeyCode::Tab));
        assert_eq!(app.editor.text(), "see src/lib.rs ");
        assert!(app.at_matches().is_empty());
        // Enter inserts too (no submit) and Down selects
        let mut app = App::default();
        app.set_files(vec!["a.rs".into(), "b.rs".into()]);
        type_str(&mut app, "@");
        assert_eq!(app.at_matches().len(), 2);
        app.on_key(key(KeyCode::Down));
        assert_eq!(app.on_key(key(KeyCode::Enter)), Action::Redraw);
        assert_eq!(app.editor.text(), "b.rs ");
        // no match: Enter sends the draft as typed
        let mut app = App::default();
        app.set_files(vec!["a.rs".into()]);
        assert_eq!(typed(&mut app, "@zzz"), Action::Submit("@zzz".into()));
    }

    #[test]
    fn at_panel_caps_results_and_sanitizes_rows() {
        let mut app = App::default();
        app.set_files((0..80).map(|i| format!("f{i}.rs")).collect());
        type_str(&mut app, "@f");
        assert_eq!(app.at_matches().len(), files::MAX_RESULTS);
        assert!(app
            .at_lines(80)
            .iter()
            .all(|l| !l.chars().any(char::is_control)));
    }

    #[test]
    fn file_walk_is_shallow_skips_noise_and_unsafe_names() {
        let d = std::env::temp_dir().join(format!("tole-tui-files-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        for p in [
            "a.rs",
            "src/b.rs",
            "src/x/y/z/deep.rs",
            "target/t.rs",
            ".git/c",
            "node_modules/n.js",
        ] {
            let f = d.join(p);
            std::fs::create_dir_all(f.parent().unwrap()).unwrap();
            std::fs::write(f, "").unwrap();
        }
        std::fs::write(d.join("bad\x1bname"), "").unwrap();
        std::fs::write(d.join("two\nlines"), "").unwrap();
        assert_eq!(files::list(&d), ["a.rs", "src/b.rs"]);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn markdown_headings_code_and_inline_spans() {
        let mut t = Transcript::default();
        let out = t.push(
            Kind::Text,
            "# Title\nplain **bold** and `code`\n```rust\nlet x = 1;\n```\nafter\n",
        );
        let kinds: Vec<_> = out.iter().map(|o| (o.kind, o.text.as_str())).collect();
        assert_eq!(
            kinds,
            [
                (Kind::Heading, "Title"),
                (Kind::Text, "plain **bold** and `code`"),
                (Kind::Code, "    let x = 1;"),
                (Kind::Text, "after"),
            ]
        );
        let spans = inline_spans("plain **bold** and `code` **open", &Theme::default());
        let shown: Vec<_> = spans.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(shown, ["plain ", "bold", " and ", "code", " **open"]);
        assert!(spans[1].style.add_modifier.contains(Modifier::BOLD));
        assert_ne!(spans[3].style, Style::new());
    }

    #[test]
    fn diff_lines_in_tool_output_are_styled_after_sanitizing() {
        let mut app = App::default();
        app.on_msg(Msg::ToolResult {
            ok: true,
            preview: "edited\n@@ -1 +1 @@\n-old\x1b[2J\n+new".into(),
        });
        app.on_key(ctrl('o'));
        let out = app.take_out();
        assert!(out.iter().all(|o| !o.text.chars().any(char::is_control)));
        let style_of = |needle: &str| {
            let o = out.iter().find(|o| o.text.contains(needle)).unwrap();
            styled(o.kind, &o.text, &Theme::default()).spans[0].style
        };
        assert_eq!(style_of("+new").fg, Some(Color::Green));
        assert_eq!(style_of("-old").fg, Some(Color::Red));
        assert!(style_of("@@").add_modifier.contains(Modifier::DIM));
    }

    /// Steering reaches the running turn through the existing #377
    /// soft-interrupt queue: the host sees the text on `cancel.interrupts()`.
    struct SteerHost;
    impl TuiHost for SteerHost {
        fn footer(&self) -> Footer {
            footer()
        }
        fn run_turn(&self, p: &str, c: &CancelToken, _o: Arc<dyn TurnObserver>) -> TurnEnd {
            if p == "first" {
                let end = Instant::now() + Duration::from_secs(3);
                while c.interrupts().is_empty() && Instant::now() < end {
                    std::thread::sleep(Duration::from_millis(5));
                }
                let got: Vec<_> = c.interrupts().drain().into_iter().map(|i| i.text).collect();
                return TurnEnd::Final(format!("steered-with {}", got.join("+")));
            }
            std::thread::sleep(Duration::from_millis(60));
            TurnEnd::Final(format!("ran {p}"))
        }
    }

    fn run_script(host: &dyn TuiHost, script: Vec<Option<Event>>) -> String {
        let mut term = Terminal::with_options(
            TestBackend::new(80, 14),
            TerminalOptions {
                viewport: Viewport::Inline(VIEWPORT_HEIGHT),
            },
        )
        .unwrap();
        let (_p, rx) = approval_channel();
        let mut it = script.into_iter();
        event_loop(&mut term, host, &mut App::new(footer()), &rx, |_| {
            std::thread::sleep(Duration::from_millis(5));
            Ok(it.next().unwrap_or(None))
        })
        .unwrap();
        format!("{}", term.backend())
    }
    fn keys(s: &str) -> Vec<Option<Event>> {
        s.chars()
            .map(|c| Some(Event::Key(key(KeyCode::Char(c)))))
            .collect()
    }

    #[test]
    fn loop_steer_reaches_the_interrupt_queue_of_the_running_turn() {
        let mut script = keys("first");
        script.push(Some(Event::Key(key(KeyCode::Enter))));
        script.extend(keys("go left"));
        script.push(Some(Event::Key(key(KeyCode::Enter))));
        script.extend(std::iter::repeat_n(None, 40));
        script.push(Some(Event::Key(ctrl('c'))));
        script.push(Some(Event::Key(ctrl('c'))));
        let text = run_script(&SteerHost, script);
        assert!(text.contains("steered-with go left"), "{text}");
    }

    #[test]
    fn loop_runs_queued_follow_ups_after_the_turn_settles_in_order() {
        let mut script = keys("slowish");
        script.push(Some(Event::Key(key(KeyCode::Enter))));
        for m in ["q1", "q2"] {
            script.extend(keys(m));
            script.push(Some(Event::Key(alt(KeyCode::Enter))));
        }
        script.extend(std::iter::repeat_n(None, 120));
        script.push(Some(Event::Key(ctrl('c'))));
        script.push(Some(Event::Key(ctrl('c'))));
        let text = run_script(&SteerHost, script);
        let (a, b) = (text.find("ran q1"), text.find("ran q2"));
        assert!(a.is_some() && b.is_some() && a < b, "{text}");
    }
}
