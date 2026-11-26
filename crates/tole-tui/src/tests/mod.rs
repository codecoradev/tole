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
mod event_loop;
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
