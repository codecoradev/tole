//! Inline terminal UI for tole: presentation and event loop only.
//!
//! Depends on `tole-core`, never on `tole-cli` (crate cycle); the CLI hands
//! config, approver, provider and storage over through [`TuiHost`].
//! Rendering is a ratatui INLINE viewport (no alternate screen), so the
//! terminal scrollback keeps the conversation after exit.

use std::io::{self, Stdout};

use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use crossterm::terminal::{disable_raw_mode, enable_raw_mode};
use ratatui::backend::{Backend, CrosstermBackend};
use ratatui::layout::Rect;
use ratatui::text::Line;
use ratatui::widgets::Paragraph;
use ratatui::{Frame, Terminal, TerminalOptions, Viewport};
use tole_core::approval::Approver;

/// Rows the inline viewport occupies.
const VIEWPORT_HEIGHT: u16 = 3;

/// What the CLI supplies to the TUI. Deliberately tiny; #460 grows it
/// (provider and storage accessors land with the transcript/streaming work).
pub trait TuiHost {
    /// One-line description of the host (model, session dir, ...) for the header.
    fn label(&self) -> String;
    /// The approver every non-ReadOnly tool call must go through.
    fn approver(&self) -> &dyn Approver;
}

/// What a key press did to the loop.
#[derive(Debug, PartialEq, Eq)]
pub enum Action {
    None,
    Redraw,
    Quit,
}

/// Quit-line state: `Ctrl+C` twice or `/quit` + Enter.
#[derive(Default)]
pub struct App {
    line: String,
    armed: bool,
}

impl App {
    pub fn on_key(&mut self, key: KeyEvent) -> Action {
        if key.kind == KeyEventKind::Release {
            return Action::None;
        }
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            if self.armed {
                return Action::Quit;
            }
            self.armed = true;
            return Action::Redraw;
        }
        self.armed = false;
        match key.code {
            KeyCode::Char(c) => self.line.push(c),
            KeyCode::Backspace => {
                self.line.pop();
            }
            KeyCode::Enter => {
                if self.line.trim() == "/quit" {
                    return Action::Quit;
                }
                self.line.clear();
            }
            _ => return Action::None,
        }
        Action::Redraw
    }

    pub fn draw(&self, host: &dyn TuiHost, f: &mut Frame) {
        let a = f.area();
        let hint = if self.armed {
            "press Ctrl+C again to quit"
        } else {
            "Ctrl+C twice or /quit to exit"
        };
        let lines = vec![
            Line::from(format!("tole tui — skeleton  [{}]", host.label())),
            Line::from(hint),
            Line::from(format!("> {}", self.line)),
        ];
        f.render_widget(
            Paragraph::new(lines),
            Rect {
                height: VIEWPORT_HEIGHT.min(a.height),
                ..a
            },
        );
    }
}

/// Raw mode on for the guard's lifetime; Drop restores it even when a panic
/// unwinds through `run`. The inline viewport leaves the cursor below the
/// frame, so scrollback stays intact — nothing else to undo.
pub struct TerminalGuard;

impl TerminalGuard {
    pub fn enter() -> io::Result<Self> {
        enable_raw_mode()?;
        Ok(Self)
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
    }
}

/// Run the TUI until quit. SIGINT is a plain `Ctrl+C` key event in raw mode.
pub fn run(host: &dyn TuiHost) -> io::Result<()> {
    let _guard = TerminalGuard::enter()?;
    let mut term: Terminal<CrosstermBackend<Stdout>> = Terminal::with_options(
        CrosstermBackend::new(io::stdout()),
        TerminalOptions {
            viewport: Viewport::Inline(VIEWPORT_HEIGHT),
        },
    )?;
    let mut app = App::default();
    event_loop(&mut term, host, &mut app, event::read)?;
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

/// Backend- and source-agnostic loop (unit-testable with `TestBackend`).
pub fn event_loop<B: Backend>(
    term: &mut Terminal<B>,
    host: &dyn TuiHost,
    app: &mut App,
    mut next: impl FnMut() -> io::Result<Event>,
) -> io::Result<()>
where
    B::Error: std::fmt::Display,
{
    let draw = |term: &mut Terminal<B>, app: &App| {
        term.draw(|f| app.draw(host, f))
            .map(|_| ())
            .map_err(io_err::<B>)
    };
    draw(term, app)?;
    loop {
        match next()? {
            Event::Key(k) => match app.on_key(k) {
                Action::Quit => return Ok(()),
                Action::Redraw => draw(term, app)?,
                Action::None => {}
            },
            Event::Resize(..) => {
                term.autoresize().map_err(io_err::<B>)?;
                draw(term, app)?;
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::backend::TestBackend;
    use tole_core::approval::{ToolRequest, Verdict};

    struct Host;
    struct Deny;
    impl Approver for Deny {
        fn decide(&self, _: &ToolRequest<'_>) -> Verdict {
            Verdict::Deny
        }
        fn interactive(&self) -> bool {
            false
        }
    }
    impl TuiHost for Host {
        fn label(&self) -> String {
            "test".into()
        }
        fn approver(&self) -> &dyn Approver {
            &Deny
        }
    }

    fn ctrl_c() -> KeyEvent {
        KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)
    }
    fn key(c: KeyCode) -> KeyEvent {
        KeyEvent::new(c, KeyModifiers::NONE)
    }

    #[test]
    fn ctrl_c_twice_quits_but_once_only_arms() {
        let mut app = App::default();
        assert_eq!(app.on_key(ctrl_c()), Action::Redraw);
        assert_eq!(app.on_key(ctrl_c()), Action::Quit);
    }

    #[test]
    fn other_key_disarms_ctrl_c() {
        let mut app = App::default();
        app.on_key(ctrl_c());
        app.on_key(key(KeyCode::Char('x')));
        assert_eq!(app.on_key(ctrl_c()), Action::Redraw);
    }

    #[test]
    fn slash_quit_enter_quits() {
        let mut app = App::default();
        for c in "/quit".chars() {
            app.on_key(key(KeyCode::Char(c)));
        }
        assert_eq!(app.on_key(key(KeyCode::Enter)), Action::Quit);
    }

    #[test]
    fn other_line_does_not_quit() {
        let mut app = App::default();
        app.on_key(key(KeyCode::Char('a')));
        assert_eq!(app.on_key(key(KeyCode::Enter)), Action::Redraw);
    }

    #[test]
    fn loop_renders_survives_resize_and_quits() {
        let mut term = Terminal::with_options(
            TestBackend::new(40, 10),
            TerminalOptions {
                viewport: Viewport::Inline(VIEWPORT_HEIGHT),
            },
        )
        .unwrap();
        let mut events = vec![
            Event::Resize(30, 10),
            Event::Key(ctrl_c()),
            Event::Key(ctrl_c()),
        ]
        .into_iter();
        event_loop(&mut term, &Host, &mut App::default(), || {
            Ok(events.next().unwrap())
        })
        .unwrap();
        let text = format!("{}", term.backend());
        assert!(text.contains("tole tui — skeleton"), "{text}");
    }
}
