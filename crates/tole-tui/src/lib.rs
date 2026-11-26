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
mod files;
mod host;
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

use std::io::{self, Stdout};
use std::sync::mpsc::{self, Receiver};
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
use tole_core::cancel::CancelToken;
use tole_core::turn::TurnObserver;

pub use app::Action;
pub use approval::{approval_channel, ApprovalRequest, Pending, Prompter};
pub use editor::Editor;
pub use host::{Footer, HostCmd, HostReply, ImageItem, SessionItem, TuiHost, TurnEnd};
pub use input::{Input, Key, KeyPress, Mods};
pub use keymap::{KeyAction, KeySpec, Keymap};
pub use menu::{Item, PickKind};
pub use observer::tool_line;
use observer::ChanObserver;
use render::{styled, wrap};
pub(crate) use sanitize::{clip, one_line, sanitize};
pub use theme::{parse_color, parse_spec, Role, Theme, BUILTIN as BUILTIN_THEMES};
pub use transcript::{Kind, Out, Transcript};

/// live area (5: stream + hint, or a picker/menu) + editor (top border + 4) + footer (1).
const LIVE_ROWS: u16 = 5;
const VIEWPORT_HEIGHT: u16 = LIVE_ROWS + 1 + 4 + 1;
const EDITOR_ROWS: usize = 4;

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

impl App {
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
    let mut app = App::with_theme(host.footer(), host.theme()).with_keymap(host.keymap());
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
mod tests;
