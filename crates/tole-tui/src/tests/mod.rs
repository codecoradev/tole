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
    let (notes, stale, _) = host_says(HostCmd::Fork(None), Ok(HostReply::Session("s-fork".into())));
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
