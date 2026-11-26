use super::*;

#[test]
fn a_paste_is_one_edit_that_never_submits_or_steers() {
    let mut app = App::default();
    typed_input(&mut app, "intro ");
    assert_eq!(
        app.on_input(Input::Paste("one\r\ntwo\rthree\x1b[2J\n".into())),
        Action::Redraw,
        "the newlines in a paste do not submit"
    );
    assert_eq!(app.editor.text(), "intro one\ntwo\nthree[2J\n");
    assert!(!app.running());
    assert_eq!(
        app.on_input(Input::new(Key::Enter)),
        Action::Submit("intro one\ntwo\nthree[2J".into())
    );
    // While a turn runs a paste is still only an edit: Enter steers.
    assert_eq!(
        app.on_input(Input::Paste("more\nlines".into())),
        Action::Redraw
    );
    assert_eq!(app.editor.text(), "more\nlines");
    assert_eq!(
        app.on_input(Input::new(Key::Enter)),
        Action::Steer("more\nlines".into())
    );
    // An empty (or all-control) paste changes nothing.
    assert_eq!(app.on_input(Input::Paste("\x1b\x07".into())), Action::None);
}

#[test]
fn a_paste_never_answers_an_approval_and_disarms_ctrl_c() {
    let mut app = App::default();
    let (tx, rx) = mpsc::channel();
    app.on_msg(Msg::Approval(Pending::new(
        ApprovalRequest::new("write_file a", Risk::Write),
        move |v| {
            let _ = tx.send(v);
        },
    )));
    assert_eq!(app.on_input(Input::Paste("y".into())), Action::None);
    assert!(rx.try_recv().is_err(), "a pasted `y` is not an answer");
    assert_eq!(app.editor.text(), "", "nor typed into the draft");
    app.on_input(Input::new(Key::Char('n')));
    assert_eq!(rx.try_recv(), Ok(Verdict::Deny));

    let mut app = App::default();
    assert_eq!(app.on_input(Input::ctrl(Key::Char('c'))), Action::Redraw);
    app.on_input(Input::Paste("x".into()));
    assert_eq!(
        app.on_input(Input::ctrl(Key::Char('c'))),
        Action::Redraw,
        "a paste disarms a pending Ctrl+C like any key"
    );
}

#[test]
fn a_paste_filters_an_open_picker() {
    let mut app = App::default();
    app.open_picker(
        PickKind::Model,
        vec![Item::new("alpha", "alpha"), Item::new("beta", "beta")],
    );
    assert_eq!(app.on_input(Input::Paste("bet\n".into())), Action::Redraw);
    assert_eq!(
        app.on_input(Input::new(Key::Enter)),
        Action::Host(HostCmd::SetModel("beta".into()))
    );
}

#[test]
fn only_a_lone_image_reference_pasted_while_idle_is_offered_to_the_host() {
    let mut app = App::default();
    assert_eq!(
        app.on_input(Input::Paste(" 'my shot.png'\r\n".into())),
        Action::PasteImage {
            spec: "my shot.png".into(),
            text: " 'my shot.png'\n".into()
        }
    );
    assert_eq!(app.editor.text(), "", "not inserted (yet)");
    for text in ["see shot.png", "a.png\nb.png", "notes.txt"] {
        assert_eq!(app.on_input(Input::Paste(text.into())), Action::Redraw);
    }
    // While a turn runs the host is busy: an image path is plain text.
    let mut app = App::default();
    typed_input(&mut app, "go");
    app.on_input(Input::new(Key::Enter));
    assert_eq!(
        app.on_input(Input::Paste("shot.png".into())),
        Action::Redraw
    );
    assert_eq!(app.editor.text(), "shot.png");
}

#[test]
fn a_pasted_image_is_attached_or_inserted_as_text_with_the_hosts_reason() {
    let host = FakeHost::answering(vec![Ok(HostReply::Images(vec![item("a.png")]))]);
    let mut app = App::default();
    attach_pasted(&host, &mut app, "a.png".into(), "a.png");
    assert_eq!(host.seen(), vec!["AttachImage(\"a.png\")"]);
    assert_eq!(app.editor.text(), "");
    assert_eq!(
        app.image_row(80).as_deref(),
        Some("image: a.png (image/png, 1000 B) · next message")
    );
    assert!(texts(&mut app)
        .iter()
        .any(|n| n == "(image attached to your next message: a.png (image/png, 1000 B))"));

    let host = FakeHost::answering(vec![Err(
        "image: /x.png: outside the workspace jail — refused".into(),
    )]);
    let mut app = App::default();
    attach_pasted(&host, &mut app, "/x.png".into(), "/x.png");
    assert_eq!(app.editor.text(), "/x.png", "inserted after all");
    assert!(app.images().is_empty());
    assert!(texts(&mut app).iter().any(|n| {
        n == "(pasted as text, not attached: image: /x.png: outside the workspace jail — refused)"
    }));
}

/// #560 through the loop: a terminal paste event becomes one host
/// attach call and the image row; a multi-line paste is only drawn.
#[test]
fn the_loop_attaches_a_pasted_image_and_draws_a_pasted_block() {
    let host = FakeHost::answering(vec![Ok(HostReply::Images(vec![item("shot.png")]))]);
    let mut term = Terminal::with_options(
        TestBackend::new(100, 14),
        TerminalOptions {
            viewport: Viewport::Inline(VIEWPORT_HEIGHT),
        },
    )
    .unwrap();
    let (_p, rx) = approval_channel();
    let mut script = vec![
        Some(Event::Paste("shot.png".into())),
        Some(Event::Paste("first\r\nsecond".into())),
    ];
    script.extend(std::iter::repeat_n(None, 3));
    let mut it = script.into_iter();
    let mut app = App::new(footer());
    event_loop(&mut term, &host, &mut app, &rx, |_| {
        Ok(it.next().unwrap_or(Some(Event::Key(ctrl('c')))))
    })
    .unwrap();
    assert_eq!(host.seen(), vec!["AttachImage(\"shot.png\")"]);
    assert_eq!(app.editor.text(), "first\nsecond");
    let text = format!("{}", term.backend());
    assert!(
        text.contains("image: shot.png (image/png, 1000 B) · next message"),
        "{text}"
    );
    assert!(
        text.contains("> first") && text.contains("  second"),
        "{text}"
    );
}
