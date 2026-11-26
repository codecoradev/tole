//! The terminal loop: `run` sets the terminal up, `event_loop` is the
//! backend- and source-agnostic loop (unit-testable with `TestBackend`) and
//! `commit` writes finished lines to the scrollback.

use std::io::{self, Stdout};
use std::sync::mpsc::{self, Receiver};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crossterm::event::{self, Event};
use ratatui::backend::{Backend, CrosstermBackend};
use ratatui::text::Line;
use ratatui::widgets::{Paragraph, Widget as _};
use ratatui::{Terminal, TerminalOptions, Viewport};
use tole_core::cancel::CancelToken;
use tole_core::turn::TurnObserver;

use crate::host_cmd::{attach_pasted, run_host_cmd};
use crate::observer::ChanObserver;
use crate::render::{styled, wrap};
use crate::{
    clipboard, term_input, Action, App, HostCmd, HostReply, Msg, Pending, TerminalGuard, TuiHost,
    TurnEnd, VIEWPORT_HEIGHT,
};

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
