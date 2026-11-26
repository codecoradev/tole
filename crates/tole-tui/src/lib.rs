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

use std::io::{self, Stdout};
use std::sync::mpsc::{self, Receiver};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crossterm::event::{self, Event};
use ratatui::backend::{Backend, CrosstermBackend};
use ratatui::text::Line;
use ratatui::widgets::Paragraph;
use ratatui::{Terminal, TerminalOptions, Viewport};
use tole_core::cancel::CancelToken;
use tole_core::turn::TurnObserver;

pub use app::{Action, Msg};
pub use approval::{approval_channel, ApprovalRequest, Pending, Prompter};
pub use editor::Editor;
pub use guard::TerminalGuard;
pub use host::{Footer, HostCmd, HostReply, ImageItem, SessionItem, TuiHost, TurnEnd};
use host_cmd::{attach_pasted, run_host_cmd};
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
