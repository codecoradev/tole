//! Inline terminal UI for tole: presentation and event loop only.
//!
//! Depends on `tole-core`, never on `tole-cli` (crate cycle); the CLI hands
//! the session over through [`TuiHost`] (it owns provider, storage, config and
//! runs one turn) and the approval prompt through [`approval_channel`].
//! Rendering is a ratatui INLINE viewport (no alternate screen): finished
//! lines are committed to the terminal scrollback, the viewport only holds the
//! live stream line, the editor and the footer.

mod editor;
mod files;
mod menu;
mod sanitize;
mod transcript;

use std::io::{self, Stdout};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crossterm::event::{
    self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, KeyboardEnhancementFlags,
    PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
};
use crossterm::terminal::{disable_raw_mode, enable_raw_mode};
use crossterm::{execute, queue};
use ratatui::backend::{Backend, CrosstermBackend};
use ratatui::layout::{Constraint, Layout, Position};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};
use ratatui::{Frame, Terminal, TerminalOptions, Viewport};
use serde_json::Value;
use tole_core::approval::Verdict;
use tole_core::cancel::CancelToken;
use tole_core::turn::TurnObserver;

pub use editor::Editor;
pub use menu::{Item, PickKind};
pub(crate) use sanitize::{one_line, sanitize};
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
    /// Run one full turn (blocking). Streamed output goes through `observer`;
    /// `cancel` is set by Esc.
    fn run_turn(
        &self,
        prompt: &str,
        cancel: &CancelToken,
        observer: Arc<dyn TurnObserver>,
    ) -> TurnEnd;

    // Slash-command surface (#461). Called only while no turn runs, so a
    // host may take its session lock without blocking the UI.
    /// Model ids for the picker (`/models`).
    fn models(&self) -> Result<Vec<String>, String> {
        unsupported()
    }
    /// Pin the session model (`fact/model`); applies from the next call.
    fn set_model(&self, _id: &str) -> Result<(), String> {
        unsupported()
    }
    /// Set (`Some`) or clear (`None`) the session reasoning effort.
    fn set_effort(&self, _level: Option<&str>) -> Result<(), String> {
        unsupported()
    }
    /// Ask for a compaction pass before the next prompt.
    fn request_compaction(&self) -> Result<(), String> {
        unsupported()
    }
    /// Start a fresh session; returns its id.
    fn new_session(&self) -> Result<String, String> {
        unsupported()
    }
    fn sessions(&self) -> Result<Vec<SessionItem>, String> {
        unsupported()
    }
    /// Swap the session turns run against; returns its id.
    fn switch_session(&self, _id: &str) -> Result<String, String> {
        unsupported()
    }
    fn status_lines(&self) -> Vec<String> {
        Vec::new()
    }
    fn set_name(&self, _name: &str) -> Result<(), String> {
        unsupported()
    }
}

/// What the loop asks of the host for a slash command or hotkey (#461). The
/// host applies it to the session (registers, storage); the loop then
/// refreshes the footer, so a switch shows on the next draw.
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
    Status,
    Name(String),
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

/// A tool call waiting for y/n. Dropping it unanswered denies (fail closed).
pub struct Pending {
    text: String,
    destructive: bool,
    reply: Sender<Verdict>,
}

/// Handed to the host's approver; blocks the turn thread until the UI answers.
#[derive(Clone)]
pub struct Prompter(Arc<Mutex<Sender<Pending>>>);

impl Prompter {
    pub fn ask(&self, text: String, destructive: bool) -> Verdict {
        let text = one_line(&text);
        let (reply, answer) = mpsc::channel();
        let sent = self
            .0
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .send(Pending {
                text,
                destructive,
                reply,
            });
        if sent.is_err() {
            return Verdict::Deny;
        }
        answer.recv().unwrap_or(Verdict::Deny)
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
        let mut rows: Vec<String> = self
            .queue
            .iter()
            .take(QUEUE_ROWS)
            .map(|q| clip(&format!("queued: {}", one_line(q)), width.saturating_sub(1)))
            .collect();
        if self.queue.len() > QUEUE_ROWS {
            rows[QUEUE_ROWS - 1] = clip(
                &format!("+{} more queued", self.queue.len() - QUEUE_ROWS + 1),
                width.saturating_sub(1),
            );
        }
        rows
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

    fn picker_key(&mut self, code: KeyCode) -> Action {
        let Some(p) = self.picker.as_mut() else {
            return Action::None;
        };
        match code {
            KeyCode::Esc => self.picker = None,
            KeyCode::Up => p.up(),
            KeyCode::Down => p.down(),
            KeyCode::Backspace => p.backspace(),
            KeyCode::Char(c) => p.type_char(c),
            KeyCode::Enter => {
                let kind = p.kind;
                let chosen = p.chosen();
                self.picker = None;
                return match (kind, chosen) {
                    (PickKind::Model, Some(id)) => {
                        self.note_model_chosen(&id);
                        Action::Host(HostCmd::SetModel(id))
                    }
                    (PickKind::Session, Some(id)) => Action::Host(HostCmd::Switch(id)),
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
            "/session" => host(HostCmd::Status),
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

    pub fn on_key(&mut self, key: KeyEvent) -> Action {
        if key.kind == KeyEventKind::Release {
            return Action::None;
        }
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        if ctrl && key.code == KeyCode::Char('c') {
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
            return self.answer(p, key.code);
        }
        if self.picker.is_some() {
            return self.picker_key(key.code);
        }
        // Hotkeys wait for idle (they take the session lock). Shift+Tab is
        // `BackTab`, a different key code from Enter, so Shift+Enter keeps
        // its newline meaning below.
        match key.code {
            KeyCode::Char('o') if ctrl => {
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
            KeyCode::Char('t') if ctrl => {
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
            KeyCode::Char('l') if ctrl => {
                return if self.running {
                    Action::None
                } else {
                    Action::Host(HostCmd::PickModel)
                }
            }
            KeyCode::Char('p') if ctrl => {
                return if self.running {
                    Action::None
                } else {
                    self.cycle_model()
                }
            }
            KeyCode::BackTab => {
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
            match key.code {
                KeyCode::Tab | KeyCode::Enter if self.at_insert() => return Action::Redraw,
                KeyCode::Up if !key.modifiers.contains(KeyModifiers::ALT) => {
                    self.at_sel = self.at_sel.min(n - 1).saturating_sub(1);
                    return Action::Redraw;
                }
                KeyCode::Down => {
                    self.at_sel = (self.at_sel + 1).min(n - 1);
                    return Action::Redraw;
                }
                _ => {}
            }
        }
        if key.code == KeyCode::Up && key.modifiers.contains(KeyModifiers::ALT) {
            return match self.queue.pop() {
                Some(last) => {
                    self.prepend_draft(vec![last]);
                    Action::Redraw
                }
                None => Action::None,
            };
        }
        let menu = self.menu();
        match key.code {
            KeyCode::Tab if !menu.is_empty() => {
                let pick = menu[self.menu_sel.min(menu.len() - 1)];
                self.editor.set(&format!("{pick} "));
                self.menu_sel = 0;
            }
            KeyCode::Up if !menu.is_empty() => self.menu_sel = self.menu_sel.saturating_sub(1),
            KeyCode::Down if !menu.is_empty() => {
                self.menu_sel = (self.menu_sel + 1).min(menu.len() - 1)
            }
            KeyCode::Esc if self.running => return self.cancel(),
            // Alt+Enter while a turn runs queues a follow-up; idle it is a newline.
            KeyCode::Enter if self.running && key.modifiers.contains(KeyModifiers::ALT) => {
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
            KeyCode::Enter
                if key
                    .modifiers
                    .intersects(KeyModifiers::SHIFT | KeyModifiers::ALT) =>
            {
                self.editor.newline()
            }
            KeyCode::Enter => {
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
                        let echoed = self.transcript.line(Kind::User, &format!("> {text}"));
                        self.out.extend(echoed);
                        Action::Submit(text)
                    }
                    None => Action::Redraw,
                };
            }
            // Fallback for terminals that cannot report Shift+Enter.
            KeyCode::Char('j') if ctrl => self.editor.newline(),
            KeyCode::Char(c) if !ctrl => {
                self.menu_sel = 0;
                self.at_sel = 0;
                if c == '@' && self.files.is_none() {
                    self.files = Some(
                        std::env::current_dir().map_or_else(|_| Vec::new(), |d| files::list(&d)),
                    );
                }
                self.editor.insert(c)
            }
            KeyCode::Backspace => {
                self.menu_sel = 0;
                self.at_sel = 0;
                self.editor.backspace()
            }
            KeyCode::Up => self.editor.prev(),
            KeyCode::Down => self.editor.next(),
            _ => return Action::None,
        }
        Action::Redraw
    }

    /// y/n answer; Esc denies AND cancels the turn; anything else re-asks.
    fn answer(&mut self, p: Pending, code: KeyCode) -> Action {
        let (verdict, note) = match code {
            KeyCode::Char('y' | 'Y') => (Verdict::Allow, "allowed"),
            KeyCode::Char('n' | 'N') => (Verdict::Deny, "denied"),
            KeyCode::Esc => {
                let _ = p.reply.send(Verdict::Deny);
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
        let _ = p.reply.send(verdict);
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
                let tag = if p.destructive {
                    "destructive"
                } else {
                    "write"
                };
                v.extend(
                    self.transcript
                        .line(Kind::Note, &format!("? {tag}: {}", p.text)),
                );
                self.approval = Some(p);
                v
            }
            Msg::Done(end) => {
                self.running = false;
                self.approval = None;
                self.footer_stale = true;
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

    pub fn draw(&self, f: &mut Frame) {
        let area = f.area();
        let [live, edit_box, foot] = Layout::vertical([
            Constraint::Length(LIVE_ROWS),
            Constraint::Length(EDITOR_ROWS as u16 + 1),
            Constraint::Length(1),
        ])
        .areas(area);
        // The border shows the session's reasoning level (#461).
        let level = self.footer.effort.as_deref().unwrap_or("-");
        let block = Block::new()
            .borders(Borders::TOP)
            .title(one_line(&format!(" thinking: {level} ")))
            .border_style(Style::new().add_modifier(Modifier::DIM));
        let edit = block.inner(edit_box);
        f.render_widget(block, edit_box);
        let w = area.width as usize;

        let mut live_lines = Vec::new();
        let menu = self.menu();
        let at = self.at_lines(w);
        if let Some(p) = &self.picker {
            live_lines.extend(p.lines(w).into_iter().map(Line::from));
        } else if let Some(p) = &self.approval {
            live_lines.push(Line::styled(
                format!("? {}", clip(&p.text, w.saturating_sub(2))),
                Style::new().fg(Color::Yellow),
            ));
            live_lines.push(Line::from("[y] allow  [n] deny  [Esc] deny + cancel"));
        } else if !at.is_empty() {
            live_lines.extend(
                at.into_iter()
                    .map(|l| Line::styled(l, Style::new().fg(Color::Cyan))),
            );
            live_lines.push(Line::styled(
                "Tab/Enter inserts the path · Up/Down select · keep typing to filter",
                Style::new().add_modifier(Modifier::DIM),
            ));
        } else if !menu.is_empty() {
            let sel = self.menu_sel.min(menu.len() - 1);
            for (i, name) in menu.iter().enumerate().take(LIVE_ROWS as usize) {
                let help = menu::COMMANDS
                    .iter()
                    .find(|c| c.0 == *name)
                    .map_or("", |c| c.1);
                let mark = if i == sel { "▸ " } else { "  " };
                live_lines.push(Line::styled(
                    clip(&format!("{mark}{name}  {help}"), w),
                    Style::new().fg(Color::Cyan),
                ));
            }
            live_lines.push(Line::styled(
                "Tab completes · Up/Down select · Enter runs",
                Style::new().add_modifier(Modifier::DIM),
            ));
        } else {
            let partial = self
                .transcript
                .partial()
                .map(|(k, t)| styled(k, &tail(t, w)))
                .unwrap_or_default();
            live_lines.push(partial);
            live_lines.extend(
                self.queue_rows(w)
                    .into_iter()
                    .map(|r| Line::styled(r, Style::new().fg(Color::Magenta))),
            );
            live_lines.push(Line::styled(
                if self.running {
                    "working… Enter steers · Alt+Enter queues · Alt+Up unqueues · Esc cancels"
                } else if self.armed {
                    "press Ctrl+C again to quit"
                } else {
                    "Enter sends · / commands · @ file · Ctrl+L model · Shift+Tab thinking · Shift+Enter newline"
                },
                Style::new().add_modifier(Modifier::DIM),
            ));
        }
        f.render_widget(Paragraph::new(live_lines), live);

        let lines = self.editor.lines();
        let shown = &lines[lines.len().saturating_sub(EDITOR_ROWS)..];
        let first = lines.len() - shown.len();
        let rows: Vec<Line> = shown
            .iter()
            .enumerate()
            .map(|(i, l)| Line::from(format!("{}{l}", if first + i == 0 { "> " } else { "  " })))
            .collect();
        f.render_widget(Paragraph::new(rows), edit);
        let last = shown.last().map_or(0, |l| l.chars().count()) + 2;
        f.set_cursor_position(Position::new(
            edit.x + (last as u16).min(edit.width.saturating_sub(1)),
            edit.y + shown.len().saturating_sub(1) as u16,
        ));

        f.render_widget(
            Paragraph::new(Line::styled(
                self.footer.render(w),
                Style::new().add_modifier(Modifier::DIM),
            )),
            foot,
        );
    }
}

/// The last `width` chars: the live line shows the newest part of the stream.
fn tail(s: &str, width: usize) -> String {
    let n = s.chars().count();
    s.chars().skip(n.saturating_sub(width)).collect()
}

fn styled(kind: Kind, text: &str) -> Line<'static> {
    let style = match kind {
        Kind::User => Style::new().add_modifier(Modifier::BOLD),
        Kind::Text => Style::new(),
        Kind::Reasoning => Style::new().fg(Color::DarkGray).add_modifier(Modifier::DIM),
        Kind::Tool => Style::new().fg(Color::Cyan),
        Kind::ToolOut => diff_style(text),
        Kind::Heading => Style::new().fg(Color::White).add_modifier(Modifier::BOLD),
        Kind::Code => Style::new().add_modifier(Modifier::DIM),
        Kind::Note => Style::new().fg(Color::Yellow),
    };
    if kind == Kind::Text {
        return Line::from(inline_spans(text));
    }
    Line::from(Span::styled(text.to_string(), style))
}

/// Unified-diff rows in a tool result: `+` green, `-` red, `@@`/headers dim.
/// The text is already sanitized by the transcript.
fn diff_style(text: &str) -> Style {
    let t = text.trim_start();
    if t.starts_with("+++") || t.starts_with("---") || t.starts_with("@@") {
        Style::new().add_modifier(Modifier::DIM)
    } else if t.starts_with('+') {
        Style::new().fg(Color::Green)
    } else if t.starts_with('-') {
        Style::new().fg(Color::Red)
    } else {
        Style::new().fg(Color::Cyan).add_modifier(Modifier::DIM)
    }
}

/// Minimal inline markdown: `**bold**` and `` `code` ``; an unclosed marker
/// stays literal. (Not a renderer: no italics, links, lists or tables.)
fn inline_spans(text: &str) -> Vec<Span<'static>> {
    let mut out = Vec::new();
    let mut plain = String::new();
    let mut rest = text;
    while let Some(c) = rest.chars().next() {
        let (open, style) = if rest.starts_with("**") {
            ("**", Style::new().add_modifier(Modifier::BOLD))
        } else if c == '`' {
            ("`", Style::new().fg(Color::Cyan))
        } else {
            plain.push(c);
            rest = &rest[c.len_utf8()..];
            continue;
        };
        match rest[open.len()..].find(open) {
            Some(end) if end > 0 => {
                if !plain.is_empty() {
                    out.push(Span::raw(std::mem::take(&mut plain)));
                }
                out.push(Span::styled(
                    rest[open.len()..open.len() + end].to_string(),
                    style,
                ));
                rest = &rest[open.len() * 2 + end..];
            }
            _ => {
                plain.push_str(open);
                rest = &rest[open.len()..];
            }
        }
    }
    if !plain.is_empty() {
        out.push(Span::raw(plain));
    }
    out
}

/// Wrap a committed line to `width` chars per row (char count, not cell width).
fn wrap(text: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    let chars: Vec<char> = text.chars().collect();
    if chars.is_empty() {
        return vec![String::new()];
    }
    chars.chunks(width).map(|c| c.iter().collect()).collect()
}

/// Raw mode (+ keyboard enhancement, so Shift+Enter is reported on terminals
/// that speak the kitty protocol; others ignore the sequence) for the guard's
/// lifetime; Drop restores both even when a panic unwinds through `run`.
pub struct TerminalGuard;

impl TerminalGuard {
    pub fn enter() -> io::Result<Self> {
        enable_raw_mode()?;
        let mut out = io::stdout();
        let _ = queue!(
            out,
            PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES)
        );
        let _ = io::Write::flush(&mut out);
        Ok(Self)
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = execute!(io::stdout(), PopKeyboardEnhancementFlags);
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
    let mut app = App::new(host.footer());
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
    let lines: Vec<Line> = out
        .iter()
        .flat_map(|o| {
            wrap(&o.text, width)
                .into_iter()
                .map(move |t| styled(o.kind, &t))
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
    let done = |app: &mut App, r: Result<String, String>| match r {
        Ok(m) => {
            app.footer_stale = true;
            app.note(&format!("({m})"));
        }
        Err(e) => app.note(&format!("(failed: {e})")),
    };
    match cmd {
        HostCmd::PickModel => match host.models() {
            Ok(ids) if ids.is_empty() => {
                app.note("(the provider lists no models; use /model <id>)")
            }
            Ok(ids) => app.open_picker(
                PickKind::Model,
                ids.into_iter().map(|i| Item::new(i.clone(), i)).collect(),
            ),
            Err(e) => app.note(&format!("(models: {e}; use /model <id>)")),
        },
        HostCmd::SetModel(id) => done(app, host.set_model(&id).map(|()| format!("model → {id}"))),
        HostCmd::SetEffort(l) => {
            let shown = l.clone().unwrap_or_else(|| "unset".into());
            done(
                app,
                host.set_effort(l.as_deref())
                    .map(|()| format!("thinking → {shown}")),
            )
        }
        HostCmd::Compact(instructions) => {
            let extra = if instructions.is_some() {
                "; instructions are not supported yet and were ignored"
            } else {
                ""
            };
            done(
                app,
                host.request_compaction()
                    .map(|()| format!("compaction runs before your next prompt{extra}")),
            )
        }
        HostCmd::New => done(
            app,
            host.new_session().map(|id| format!("new session {id}")),
        ),
        HostCmd::PickSession => match host.sessions() {
            Ok(v) if v.is_empty() => app.note("(no other sessions)"),
            Ok(v) => app.open_picker(
                PickKind::Session,
                v.into_iter().map(|s| Item::new(s.id, s.label)).collect(),
            ),
            Err(e) => app.note(&format!("(sessions: {e})")),
        },
        HostCmd::Switch(id) => done(
            app,
            host.switch_session(&id).map(|id| format!("resumed {id}")),
        ),
        HostCmd::Status => {
            for l in host.status_lines() {
                app.note(&l);
            }
        }
        HostCmd::Name(n) => done(app, host.set_name(&n).map(|()| format!("named {n}"))),
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
            match ev {
                Event::Key(k) => match app.on_key(k) {
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
                    Action::Host(cmd) => run_host_cmd(host, app, cmd),
                    Action::Redraw | Action::None => {}
                },
                Event::Resize(..) => term.autoresize().map_err(io_err::<B>)?,
                _ => {}
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::backend::TestBackend;
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
        let (reply, rx) = mpsc::channel();
        app.on_msg(Msg::Approval(Pending {
            text: "write_file out.txt".into(),
            destructive,
            reply,
        }));
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
        assert!(rx.recv().is_err(), "dropped sender = Prompter sees Deny");
    }

    #[test]
    fn prompter_denies_when_ui_is_gone() {
        let (p, rx) = approval_channel();
        drop(rx);
        assert_eq!(p.ask("x".into(), false), Verdict::Deny);
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
        let (reply, _rx) = mpsc::channel();
        app.on_msg(Msg::Approval(Pending {
            text: "w\x1b[2K".into(),
            destructive: false,
            reply,
        }));
        for o in app.take_out() {
            assert!(!o.text.chars().any(|c| c.is_control()), "{:?}", o.text);
        }
        let f = Footer {
            model: Some("m\x1b[2J".into()),
            ..footer()
        };
        assert!(!f.render(200).chars().any(|c| c.is_control()));
        let (p, rx) = approval_channel();
        std::thread::spawn(move || p.ask("a\x1b\nb".into(), false));
        assert_eq!(rx.recv().unwrap().text, "a\\u{1b} ⏎ b");
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
        assert_eq!(app.menu().len(), 8);
        type_str(&mut app, "co");
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
        fn set_effort(&self, level: Option<&str>) -> Result<(), String> {
            *self.0.lock().unwrap() = level.map(str::to_string);
            Ok(())
        }
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
        let spans = inline_spans("plain **bold** and `code` **open");
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
            styled(o.kind, &o.text).spans[0].style
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
