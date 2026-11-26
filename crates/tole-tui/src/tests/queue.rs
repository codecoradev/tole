use super::*;

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
