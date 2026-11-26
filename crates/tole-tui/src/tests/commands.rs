use super::*;

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
