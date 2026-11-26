use super::*;

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
