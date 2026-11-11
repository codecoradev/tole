//! App-level keymap tests (#463): dispatch through `App::on_input` only, no
//! terminal types.

use super::*;
use std::sync::mpsc;

fn keymap(items: &[(&str, &[&str])]) -> Keymap {
    let over: Vec<(String, Vec<String>)> = items
        .iter()
        .map(|(a, k)| (a.to_string(), k.iter().map(|s| s.to_string()).collect()))
        .collect();
    Keymap::from_overrides(&over).unwrap()
}

fn app(km: Keymap) -> App {
    let mut a = App::new(Footer {
        model: Some("m1".into()),
        effort: Some("low".into()),
        ..Footer::default()
    })
    .with_keymap(km);
    a.recent = vec!["m1".into(), "m2".into()];
    a
}

fn notes(a: &mut App) -> String {
    a.take_out()
        .into_iter()
        .map(|o| o.text)
        .collect::<Vec<_>>()
        .join("\n")
}

fn type_str(a: &mut App, s: &str) {
    for c in s.chars() {
        a.on_input(Input::new(Key::Char(c)));
    }
}

fn pending(a: &mut App) -> mpsc::Receiver<Verdict> {
    let (tx, rx) = mpsc::channel();
    a.on_msg(Msg::Approval(Pending::new(
        ApprovalRequest::new("write_file out.txt", Risk::Write),
        move |v| {
            let _ = tx.send(v);
        },
    )));
    rx
}

fn model_changed(a: &Action) -> bool {
    matches!(a, Action::Host(HostCmd::SetModel(_)))
}

#[test]
fn the_default_keymap_dispatches_every_action_on_todays_keys() {
    let mut a = app(Keymap::default());
    a.on_input(Input::ctrl(Key::Char('o')));
    assert!(notes(&mut a).contains("(tool output expanded)"));
    a.on_input(Input::ctrl(Key::Char('t')));
    assert!(notes(&mut a).contains("(reasoning hidden)"));
    assert_eq!(
        a.on_input(Input::ctrl(Key::Char('l'))),
        Action::Host(HostCmd::PickModel)
    );
    assert!(model_changed(&a.on_input(Input::ctrl(Key::Char('p')))));
    assert_eq!(
        a.on_input(Input::new(Key::BackTab)),
        Action::Host(HostCmd::SetEffort(Some("medium".into())))
    );
    assert_eq!(
        a.on_input(Input::shift(Key::BackTab)),
        Action::Host(HostCmd::SetEffort(Some("medium".into())))
    );
    // Newline: Shift+Enter, Ctrl+J, and (idle) Alt+Enter.
    type_str(&mut a, "a");
    a.on_input(Input::shift(Key::Enter));
    a.on_input(Input::ctrl(Key::Char('j')));
    a.on_input(Input::alt(Key::Enter));
    assert_eq!(a.editor.text(), "a\n\n\n");
    // History: Up / Down.
    a.editor.submit();
    a.on_input(Input::new(Key::Up));
    assert_eq!(a.editor.text(), "a");
    a.on_input(Input::new(Key::Down));
    assert_eq!(a.editor.text(), "");
    // Queue and unqueue while a turn runs.
    a.running = true;
    type_str(&mut a, "later");
    a.on_input(Input::alt(Key::Enter));
    assert_eq!(a.queue, vec!["later".to_string()]);
    a.on_input(Input::alt(Key::Up));
    assert!(a.queue.is_empty());
    assert_eq!(a.editor.text(), "later");
}

#[test]
fn a_rebinding_acts_on_the_new_key_and_no_longer_on_the_old_one() {
    let km = keymap(&[
        ("toggle_tools", &["alt+o", "f5"]),
        ("toggle_reasoning", &["f2"]),
        ("model_picker", &["alt+m"]),
        ("cycle_model", &["alt+p"]),
        ("cycle_thinking", &["f3"]),
        ("newline", &["alt+n", "alt+j"]),
        ("queue_follow_up", &["alt+q"]),
        ("pop_queue", &["alt+u"]),
        ("history_prev", &["alt+k"]),
        ("history_next", &["alt+l"]),
    ]);
    let mut a = app(km);
    let f = |n| Input::new(Key::F(n));

    // Two keys for one action; the old key is dead.
    a.on_input(Input::alt(Key::Char('o')));
    assert!(notes(&mut a).contains("(tool output expanded)"));
    a.on_input(f(5));
    assert!(notes(&mut a).contains("(tool output collapsed)"));
    assert_eq!(a.on_input(Input::ctrl(Key::Char('o'))), Action::None);
    assert_eq!(notes(&mut a), "");

    a.on_input(f(2));
    assert!(notes(&mut a).contains("(reasoning hidden)"));
    assert_eq!(a.on_input(Input::ctrl(Key::Char('t'))), Action::None);
    assert_eq!(notes(&mut a), "");

    assert_eq!(
        a.on_input(Input::alt(Key::Char('m'))),
        Action::Host(HostCmd::PickModel)
    );
    assert_eq!(a.on_input(Input::ctrl(Key::Char('l'))), Action::None);

    assert!(model_changed(&a.on_input(Input::alt(Key::Char('p')))));
    assert_eq!(a.on_input(Input::ctrl(Key::Char('p'))), Action::None);

    assert!(matches!(
        a.on_input(f(3)),
        Action::Host(HostCmd::SetEffort(_))
    ));
    assert_eq!(a.on_input(Input::new(Key::BackTab)), Action::None);

    // Newline: new keys work; the old ones do nothing (and never send).
    type_str(&mut a, "x");
    a.on_input(Input::alt(Key::Char('n')));
    a.on_input(Input::alt(Key::Char('j')));
    assert_eq!(a.editor.text(), "x\n\n");
    assert_eq!(a.on_input(Input::shift(Key::Enter)), Action::None);
    assert_eq!(a.on_input(Input::ctrl(Key::Char('j'))), Action::None);
    assert_eq!(a.on_input(Input::alt(Key::Enter)), Action::None);
    assert_eq!(
        a.editor.text(),
        "x\n\n",
        "no old newline key, and none sent"
    );
    a.editor.submit();

    // History moves on the new keys only.
    a.on_input(Input::alt(Key::Char('k')));
    assert_eq!(a.editor.text(), "x");
    a.on_input(Input::new(Key::Down));
    assert_eq!(a.editor.text(), "x", "plain Down is no longer history");
    a.on_input(Input::alt(Key::Char('l')));
    assert_eq!(a.editor.text(), "");
    a.on_input(Input::new(Key::Up));
    assert_eq!(a.editor.text(), "");

    // Queue / unqueue while a turn runs.
    a.running = true;
    type_str(&mut a, "later");
    assert_eq!(a.on_input(Input::alt(Key::Enter)), Action::None);
    assert!(a.queue.is_empty());
    a.on_input(Input::alt(Key::Char('q')));
    assert_eq!(a.queue, vec!["later".to_string()]);
    a.on_input(Input::alt(Key::Up));
    assert_eq!(a.queue.len(), 1, "Alt+Up no longer unqueues");
    a.on_input(Input::alt(Key::Char('u')));
    assert!(a.queue.is_empty());
    assert_eq!(a.editor.text(), "later");
}

#[test]
fn a_rebound_key_never_types_or_sends_and_plain_keys_keep_working() {
    let mut a = app(keymap(&[("toggle_tools", &["alt+o"])]));
    type_str(&mut a, "hi o");
    assert_eq!(a.editor.text(), "hi o");
    a.on_input(Input::alt(Key::Char('o')));
    assert_eq!(a.editor.text(), "hi o", "the bound key is not typed");
    assert_eq!(
        a.on_input(Input::new(Key::Enter)),
        Action::Submit("hi o".into())
    );
}

#[test]
fn an_approval_prompt_ignores_every_rebinding() {
    let km = keymap(&[
        ("toggle_tools", &["alt+o", "f5"]),
        ("toggle_reasoning", &["f2"]),
        ("model_picker", &["alt+m"]),
        ("cycle_model", &["alt+p"]),
        ("cycle_thinking", &["f3"]),
        ("newline", &["alt+n"]),
        ("queue_follow_up", &["alt+q"]),
        ("pop_queue", &["alt+u"]),
        ("history_prev", &["alt+k"]),
        ("history_next", &["alt+l"]),
    ]);
    let mut a = app(km);
    a.editor.insert('d');
    a.running = true;
    let rx = pending(&mut a);
    a.take_out();
    let keys = [
        Input::alt(Key::Char('o')),
        Input::new(Key::F(5)),
        Input::new(Key::F(2)),
        Input::alt(Key::Char('m')),
        Input::alt(Key::Char('p')),
        Input::new(Key::F(3)),
        Input::alt(Key::Char('n')),
        Input::alt(Key::Char('q')),
        Input::alt(Key::Char('u')),
        Input::alt(Key::Char('k')),
        Input::alt(Key::Char('l')),
        Input::ctrl(Key::Char('l')),
        Input::new(Key::BackTab),
        Input::new(Key::Enter),
        Input::shift(Key::Enter),
        Input::new(Key::Up),
        Input::new(Key::Char('x')),
    ];
    for k in keys {
        assert_eq!(a.on_input(k.clone()), Action::None, "{k:?}");
        assert!(rx.try_recv().is_err(), "{k:?} answered the prompt");
    }
    assert_eq!(a.take_out().len(), 0, "nothing was dispatched");
    assert_eq!(a.editor.text(), "d");
    assert!(a.queue.is_empty());
    // The approval keys still work, one prompt at a time.
    a.on_input(Input::new(Key::Char('y')));
    assert_eq!(rx.try_recv(), Ok(Verdict::Allow));
    let rx = pending(&mut a);
    a.on_input(Input::new(Key::Char('n')));
    assert_eq!(rx.try_recv(), Ok(Verdict::Deny));
    let rx = pending(&mut a);
    assert_eq!(a.on_input(Input::new(Key::Esc)), Action::Cancel);
    assert_eq!(rx.try_recv(), Ok(Verdict::Deny));
    let rx = pending(&mut a);
    assert_eq!(a.on_input(Input::ctrl(Key::Char('c'))), Action::Cancel);
    assert_eq!(rx.try_recv(), Ok(Verdict::Deny));
}

#[test]
fn hints_follow_the_effective_binding() {
    let a = app(keymap(&[
        ("model_picker", &["f4"]),
        ("cycle_thinking", &["alt+t"]),
    ]));
    assert_eq!(a.keymap.label(KeyAction::ModelPicker), "F4");
    let a = app(keymap(&[("toggle_tools", &["f6"])]));
    let mut t = Transcript::default();
    t.tools_key = a.keymap.label(KeyAction::ToggleTools);
    let out = t.tool_result(true, "a\nb\nc");
    assert!(
        out.iter().any(|o| o.text.contains("[+2 lines · F6]")),
        "{out:?}"
    );
}
