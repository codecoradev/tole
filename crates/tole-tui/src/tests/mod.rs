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

mod images;
mod input;
mod paste;

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

/// #1041: a request worded by the shared summary keeps the summary, shows
/// the summary's own line and still carries the tier it was given.
#[test]
fn a_request_from_a_summary_shows_its_line_and_keeps_the_tier() {
    use tole_core::action_summary::{ActionEffect, ActionKind, ActionSummary};
    let s = ActionSummary::new(
        ActionKind::DeleteFile,
        "a\x1b[2J.txt",
        ActionEffect::CannotBeUndone,
        true,
    );
    let detail = "delete_file: delete file a.txt \x1b[31m → a.txt";
    let r = ApprovalRequest::from_summary(s.clone(), detail, Risk::Destructive);
    // The plain-language line first, then the tool's whole text, uncut.
    assert!(r.text().starts_with("delete the file a"), "{}", r.text());
    assert!(r
        .text()
        .contains("Cannot be undone. :: delete_file: delete file a.txt"));
    assert!(r.text().ends_with("→ a.txt"), "{}", r.text());
    assert!(!r.text().contains('\x1b'));
    let long = "x".repeat(5000);
    let long_req = ApprovalRequest::from_summary(s.clone(), &long, Risk::Write);
    assert!(
        long_req.text().ends_with(&long),
        "the tool's text is never cut"
    );
    assert_eq!(r.summary(), Some(&s));
    assert!(r.is_destructive() && !r.can_allow_always());
    assert!(ApprovalRequest::new("x", Risk::Write).summary().is_none());
}

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
    // Removed when the test ends, also when it fails (#781).
    struct Scratch(std::path::PathBuf);
    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    let scratch = Scratch(
        std::env::temp_dir().join(format!("tole-tui-files-{}-{stamp:x}", std::process::id())),
    );
    let d = scratch.0.clone();
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
