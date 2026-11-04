//! Inline terminal UI for tole: presentation and event loop only.
//!
//! Depends on `tole-core`, never on `tole-cli` (crate cycle); the CLI hands
//! the session over through [`TuiHost`] (it owns provider, storage, config and
//! runs one turn) and the approval prompt through [`approval_channel`].
//! Rendering is a ratatui INLINE viewport (no alternate screen): finished
//! lines are committed to the terminal scrollback, the viewport only holds the
//! live stream line, the editor and the footer.

mod editor;
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
use ratatui::widgets::Paragraph;
use ratatui::{Frame, Terminal, TerminalOptions, Viewport};
use serde_json::Value;
use tole_core::approval::Verdict;
use tole_core::cancel::CancelToken;
use tole_core::turn::TurnObserver;

pub use editor::Editor;
pub(crate) use sanitize::{one_line, sanitize};
pub use transcript::{Kind, Out, Transcript};

/// live stream (2) + editor (4) + footer (1).
const VIEWPORT_HEIGHT: u16 = 7;
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
        let first = clip(&one_line(preview.lines().next().unwrap_or("")), 80);
        self.send(Msg::Tool(format!(
            "  {} {first}",
            if ok { "✓" } else { "✗" }
        )));
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
    Cancel,
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
}

impl App {
    pub fn new(footer: Footer) -> Self {
        Self {
            footer,
            ..Self::default()
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
        match key.code {
            KeyCode::Esc if self.running => return self.cancel(),
            KeyCode::Enter
                if key
                    .modifiers
                    .intersects(KeyModifiers::SHIFT | KeyModifiers::ALT) =>
            {
                self.editor.newline()
            }
            KeyCode::Enter => {
                if self.running {
                    return Action::None;
                }
                if self.editor.text().trim() == "/quit" {
                    return Action::Quit;
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
            KeyCode::Char(c) if !ctrl => self.editor.insert(c),
            KeyCode::Backspace => self.editor.backspace(),
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
        let [live, edit, foot] = Layout::vertical([
            Constraint::Length(2),
            Constraint::Length(EDITOR_ROWS as u16),
            Constraint::Length(1),
        ])
        .areas(area);
        let w = area.width as usize;

        let mut live_lines = Vec::new();
        if let Some(p) = &self.approval {
            live_lines.push(Line::styled(
                format!("? {}", clip(&p.text, w.saturating_sub(2))),
                Style::new().fg(Color::Yellow),
            ));
            live_lines.push(Line::from("[y] allow  [n] deny  [Esc] deny + cancel"));
        } else {
            let partial = self
                .transcript
                .partial()
                .map(|(k, t)| styled(k, &tail(t, w)))
                .unwrap_or_default();
            live_lines.push(partial);
            live_lines.push(Line::styled(
                if self.running {
                    "working… (Esc cancels)"
                } else if self.armed {
                    "press Ctrl+C again to quit"
                } else {
                    "Enter sends · Shift+Enter / Ctrl+J newline · Up/Down history"
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
        Kind::Note => Style::new().fg(Color::Yellow),
    };
    Line::from(Span::styled(text.to_string(), style))
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
        loop {
            while let Ok(m) = rx.try_recv() {
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
                    Action::Submit(prompt) => {
                        cancel = CancelToken::default();
                        let (cancel, tx) = (cancel.clone(), tx.clone());
                        scope.spawn(move || {
                            let obs: Arc<dyn TurnObserver> =
                                Arc::new(ChanObserver(Mutex::new(tx.clone())));
                            let end =
                                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                                    host.run_turn(&prompt, &cancel, obs)
                                }))
                                .unwrap_or_else(|_| {
                                    TurnEnd::Stopped("turn thread panicked".into())
                                });
                            let _ = tx.send(Msg::Done(end));
                        });
                    }
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
        type_str(&mut app, "more");
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
        let Ok(Msg::Tool(done)) = rx.try_recv() else {
            panic!("no tool msg")
        };
        assert!(!done.chars().any(|c| c.is_control()), "{done:?}");
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
}
