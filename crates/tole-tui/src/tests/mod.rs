use super::*;
use crate::event_loop::event_loop;
use crate::host::unsupported;
use crate::host_cmd::{attach_pasted, run_host_cmd};
use crate::observer::ChanObserver;
use crate::render::{inline_spans, styled, wrap};
use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
use ratatui::backend::TestBackend;
use ratatui::style::{Color, Modifier, Style};
use ratatui::{Terminal, TerminalOptions, Viewport};
use std::sync::mpsc::{self, Receiver};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tole_core::approval::Verdict;
use tole_core::cancel::CancelToken;
use tole_core::tool::Risk;
use tole_core::turn::TurnObserver;

mod approval;
mod commands;
mod host;
mod images;
mod input;
mod output;
mod paste;
mod queue;

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

// ---- the UI-neutral input seam (#526): no crossterm types below ----

fn typed_input(app: &mut App, s: &str) {
    for c in s.chars() {
        app.on_input(Input::new(Key::Char(c)));
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

/// Steering reaches the running turn through the existing #377
/// soft-interrupt queue: the host sees the text on `cancel.interrupts()`.
struct SteerHost;
impl TuiHost for SteerHost {
    fn footer(&self) -> Footer {
        footer()
    }
    fn run_turn(&self, p: &str, c: &CancelToken, _o: Arc<dyn TurnObserver>) -> TurnEnd {
        if p == "first" {
            // Generous: only a run that never steers waits for it.
            let end = Instant::now() + Duration::from_secs(60);
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
