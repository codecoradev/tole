use super::*;

#[test]
fn a_draft_is_typed_and_submitted_through_inputs_alone() {
    let mut app = App::default();
    typed_input(&mut app, "hello");
    assert_eq!(
        app.on_input(Input::new(Key::Enter)),
        Action::Submit("hello".into())
    );
    assert!(app.running());
}

#[test]
fn modifiers_change_enter_exactly_as_the_key_table_says() {
    // Shift+Enter and Alt+Enter (idle) are newlines; plain Enter submits.
    let mut app = App::default();
    typed_input(&mut app, "a");
    app.on_input(Input::shift(Key::Enter));
    typed_input(&mut app, "b");
    app.on_input(Input::alt(Key::Enter));
    typed_input(&mut app, "c");
    assert_eq!(
        app.on_input(Input::new(Key::Enter)),
        Action::Submit("a\nb\nc".into())
    );
    // Ctrl+J is the newline fallback for terminals without Shift+Enter.
    let mut app = App::default();
    typed_input(&mut app, "x");
    app.on_input(Input::ctrl(Key::Char('j')));
    typed_input(&mut app, "y");
    assert_eq!(
        app.on_input(Input::new(Key::Enter)),
        Action::Submit("x\ny".into())
    );
}

#[test]
fn a_key_the_logic_ignores_still_disarms_ctrl_c() {
    // Ctrl+C once arms; any other key press disarms; Ctrl+C again only
    // arms again. This is why unmapped keys are `Key::Other`, not dropped.
    let mut app = App::default();
    assert_eq!(app.on_input(Input::ctrl(Key::Char('c'))), Action::Redraw);
    assert_eq!(app.on_input(Input::new(Key::Other)), Action::None);
    assert_eq!(app.on_input(Input::ctrl(Key::Char('c'))), Action::Redraw);
    assert_eq!(app.on_input(Input::ctrl(Key::Char('c'))), Action::Quit);
}

#[test]
fn shift_tab_is_its_own_key_not_shift_plus_tab() {
    let mut app = App::new(Footer::default());
    match app.on_input(Input::new(Key::BackTab)) {
        Action::Host(HostCmd::SetEffort(_)) => {}
        other => panic!("BackTab cycles the effort, got {other:?}"),
    }
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
    // blank and slash drafts are ignored while running (nothing to steer)
    assert_eq!(app.on_key(key(KeyCode::Enter)), Action::None);
    type_str(&mut app, "/new");
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
fn shift_tab_cycles_effort_and_shift_enter_still_newlines() {
    let mut app = App::new(Footer::default());
    let mut seq = Vec::new();
    for want in ["minimal", "low", "medium", "high"] {
        let Action::Host(HostCmd::SetEffort(Some(l))) = app.on_key(shift(KeyCode::BackTab)) else {
            panic!("no cycle")
        };
        assert_eq!(l, want);
        app.footer.effort = Some(l.clone());
        seq.push(l);
    }
    // after `high` the cycle goes to unset, then wraps to minimal
    assert_eq!(
        app.on_key(shift(KeyCode::BackTab)),
        Action::Host(HostCmd::SetEffort(None))
    );
    app.footer.effort = None;
    assert_eq!(
        app.on_key(shift(KeyCode::BackTab)),
        Action::Host(HostCmd::SetEffort(Some("minimal".into())))
    );
    // Shift+Enter is untouched: newline, never a submit or a cycle
    type_str(&mut app, "a");
    assert_eq!(app.on_key(shift(KeyCode::Enter)), Action::Redraw);
    type_str(&mut app, "b");
    assert_eq!(app.editor.text(), "a\nb");
}

#[test]
fn ctrl_p_cycles_recent_models_and_hotkeys_wait_for_idle() {
    let mut app = App::new(Footer {
        model: Some("m1".into()),
        ..Footer::default()
    });
    assert_eq!(
        app.on_key(ctrl('p')),
        Action::Redraw,
        "nothing to cycle yet"
    );
    assert!(notes(&mut app).contains("no other recent model"));
    app.note_model_chosen("m2");
    app.note_model_chosen("m3");
    let mut got = Vec::new();
    for _ in 0..3 {
        let Action::Host(HostCmd::SetModel(m)) = app.on_key(ctrl('p')) else {
            panic!("no cycle")
        };
        got.push(m);
    }
    assert_eq!(got, ["m2", "m1", "m3"]);
    type_str(&mut app, "x");
    app.on_key(key(KeyCode::Enter));
    assert!(app.running());
    for k in [ctrl('l'), ctrl('p'), shift(KeyCode::BackTab)] {
        assert_eq!(app.on_key(k), Action::None);
    }
}
