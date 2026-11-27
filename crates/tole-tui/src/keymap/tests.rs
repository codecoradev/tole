use super::*;

fn press(key: Key, ctrl: bool, alt: bool, shift: bool) -> KeyPress {
    KeyPress {
        key,
        mods: Mods { ctrl, alt, shift },
    }
}

fn over(items: &[(&str, &[&str])]) -> Vec<(String, Vec<String>)> {
    items
        .iter()
        .map(|(a, k)| (a.to_string(), k.iter().map(|s| s.to_string()).collect()))
        .collect()
}

fn err(items: &[(&str, &[&str])]) -> String {
    Keymap::from_overrides(&over(items)).unwrap_err()
}

/// Today's keys, one row per action: pin the default keymap.
#[test]
fn the_default_keymap_is_todays_behavior() {
    use Key::*;
    let k = Keymap::default();
    let table: [(KeyAction, KeyPress); 11] = [
        (KeyAction::ToggleTools, press(Char('o'), true, false, false)),
        (
            KeyAction::ToggleReasoning,
            press(Char('t'), true, false, false),
        ),
        (KeyAction::ModelPicker, press(Char('l'), true, false, false)),
        (KeyAction::CycleModel, press(Char('p'), true, false, false)),
        (KeyAction::CycleThinking, press(BackTab, false, false, true)),
        (KeyAction::Newline, press(Enter, false, false, true)),
        (KeyAction::Newline, press(Char('j'), true, false, false)),
        (KeyAction::QueueFollowUp, press(Enter, false, true, false)),
        (KeyAction::PopQueue, press(Up, false, true, false)),
        (KeyAction::HistoryPrev, press(Up, false, false, false)),
        (KeyAction::HistoryNext, press(Down, false, false, false)),
    ];
    for (action, p) in table {
        assert_eq!(k.action(&p), Some(action), "{action:?}");
    }
    // BackTab without the Shift flag is the same key.
    assert_eq!(
        k.action(&press(BackTab, false, false, false)),
        Some(KeyAction::CycleThinking)
    );
    // Letters are case-insensitive (a terminal may report Ctrl+Shift+O).
    assert_eq!(
        k.action(&press(Char('O'), true, false, true)),
        Some(KeyAction::ToggleTools)
    );
    // Keys that are not bound, and the protected ones, trigger nothing.
    for p in [
        press(Enter, false, false, false),
        press(Esc, false, false, false),
        press(Backspace, false, false, false),
        press(Char('y'), false, false, false),
        press(Char('o'), false, false, false),
        press(Char('o'), true, true, false),
        press(Char('c'), true, false, false),
    ] {
        assert_eq!(k.action(&p), None, "{p:?}");
    }
    assert_eq!(
        KeyAction::ALL.map(|a| k.keys(a).len()),
        [1, 1, 1, 1, 1, 2, 1, 1, 1, 1]
    );
    // Hints for the default keys are the texts the footer always had.
    assert_eq!(k.label(KeyAction::ModelPicker), "Ctrl+L");
    assert_eq!(k.label(KeyAction::CycleThinking), "Shift+Tab");
    assert_eq!(k.label(KeyAction::Newline), "Shift+Enter");
    assert_eq!(k.label(KeyAction::QueueFollowUp), "Alt+Enter");
    assert_eq!(k.label(KeyAction::PopQueue), "Alt+Up");
    assert_eq!(k.label(KeyAction::ToggleTools), "Ctrl+O");
}

#[test]
fn a_rebinding_replaces_the_old_keys_and_keeps_the_other_defaults() {
    let k = Keymap::from_overrides(&over(&[
        ("toggle_tools", &["alt+o", "F5"]),
        ("history_prev", &["up", "ctrl+k"]),
    ]))
    .unwrap();
    assert_eq!(
        k.action(&press(Key::Char('o'), false, true, false)),
        Some(KeyAction::ToggleTools)
    );
    assert_eq!(
        k.action(&press(Key::F(5), false, false, false)),
        Some(KeyAction::ToggleTools)
    );
    assert_eq!(k.action(&press(Key::Char('o'), true, false, false)), None);
    assert_eq!(
        k.action(&press(Key::Char('k'), true, false, false)),
        Some(KeyAction::HistoryPrev)
    );
    assert_eq!(
        k.action(&press(Key::Char('l'), true, false, false)),
        Some(KeyAction::ModelPicker)
    );
    assert_eq!(k.label(KeyAction::ToggleTools), "Alt+O");
    assert_eq!(Keymap::from_overrides(&[]).unwrap(), Keymap::default());
}

#[test]
fn specs_are_case_insensitive_and_modifier_order_free() {
    for s in ["Ctrl+Alt+X", "alt+ctrl+x", " ALT + CTRL + x "] {
        let k = Keymap::from_overrides(&over(&[("cycle_model", &[s])])).unwrap();
        assert_eq!(
            k.action(&press(Key::Char('x'), true, true, false)),
            Some(KeyAction::CycleModel),
            "{s}"
        );
    }
    let k = Keymap::from_overrides(&over(&[("newline", &["Ctrl+Enter"])])).unwrap();
    assert_eq!(
        k.action(&press(Key::Enter, true, false, false)),
        Some(KeyAction::Newline)
    );
    let k = Keymap::from_overrides(&over(&[("cycle_thinking", &["alt+Left"])])).unwrap();
    assert_eq!(
        k.action(&press(Key::Left, false, true, false)),
        Some(KeyAction::CycleThinking)
    );
}

#[test]
fn bare_characters_and_protected_keys_are_rejected() {
    for s in [
        "o",
        "y",
        "n",
        "/",
        "@",
        "9",
        "shift+o",
        "enter",
        "esc",
        "escape",
        "backspace",
        "alt+esc",
        "ctrl+backspace",
        "tab",
        "ctrl+tab",
        "alt+tab",
        "ctrl+c",
        "Ctrl+C",
        "up",
        "down",
        "left",
        "right",
        "ctrl+/",
    ] {
        assert!(
            Keymap::from_overrides(&over(&[("toggle_tools", &[s])])).is_err(),
            "`{s}` must be rejected"
        );
    }
    // Bare Up / Down only for the history pair.
    assert!(Keymap::from_overrides(&over(&[("history_next", &["up"])])).is_err());
    assert!(Keymap::from_overrides(&over(&[("history_prev", &["down"])])).is_err());
    // Function keys need no modifier.
    assert!(Keymap::from_overrides(&over(&[("toggle_tools", &["f12"])])).is_ok());
    assert!(Keymap::from_overrides(&over(&[("toggle_tools", &["f13"])])).is_err());
    assert!(Keymap::from_overrides(&over(&[("toggle_tools", &["f0"])])).is_err());
}

#[test]
fn keys_a_terminal_cannot_tell_apart_are_rejected() {
    for (s, why) in [
        ("ctrl+i", "Tab"),
        ("ctrl+m", "Enter"),
        ("ctrl+h", "Backspace"),
        ("ctrl+[", "Esc"),
        ("ctrl+s", "flow-control"),
        ("ctrl+q", "flow-control"),
        ("ctrl+z", "flow-control"),
        ("ctrl+\\", "signal"),
        ("ctrl+5", "Ctrl+digit"),
    ] {
        let e = err(&[("toggle_tools", &[s])]);
        assert!(e.contains(why) && e.contains("toggle_tools"), "{s}: {e}");
    }
    // Ctrl+J is allowed: it is the newline fallback.
    assert!(Keymap::from_overrides(&over(&[("newline", &["ctrl+j"])])).is_ok());
}

#[test]
fn a_bad_spec_names_the_action_and_the_spec() {
    for s in ["", "ctrl+", "hyper+o", "ctrl+ctrl+o", "ctrl+foo", "ctrl++"] {
        let e = err(&[("cycle_model", &[s])]);
        assert!(e.starts_with("tui.keys.cycle_model: "), "{s}: {e}");
        assert!(e.contains(&format!("{s:?}")), "{s}: {e}");
    }
    let e = err(&[("flip", &["ctrl+o"])]);
    assert!(
        e.contains("unknown action \"flip\"") && e.contains("toggle_tools"),
        "{e}"
    );
    let e = err(&[("toggle_tools", &[])]);
    assert!(
        e.contains("toggle_tools") && e.contains("at least one key"),
        "{e}"
    );
}

#[test]
fn a_key_on_two_actions_is_rejected_naming_both() {
    // Explicit twice.
    let e = err(&[("toggle_tools", &["ctrl+x"]), ("cycle_model", &["ctrl+x"])]);
    assert!(
        e.contains("Ctrl+X") && e.contains("`toggle_tools`") && e.contains("`cycle_model`"),
        "{e}"
    );
    // Onto another action's untouched default.
    let e = err(&[("cycle_model", &["ctrl+o"])]);
    assert!(
        e.contains("`toggle_tools`") && e.contains("`cycle_model`"),
        "{e}"
    );
    // Swapping two defaults is fine.
    assert!(Keymap::from_overrides(&over(&[
        ("toggle_tools", &["ctrl+t"]),
        ("toggle_reasoning", &["ctrl+o"]),
    ]))
    .is_ok());
    // One action listing the same key twice.
    let e = err(&[("toggle_tools", &["ctrl+x", "CTRL+X"])]);
    assert!(e.contains("`toggle_tools`"), "{e}");
}

/// Defense in depth: even a table that skipped the parse-time rules
/// (built here by hand) cannot take the protected keys.
#[test]
fn a_hostile_table_cannot_take_the_protected_keys() {
    let spec = |key, ctrl| KeySpec {
        key,
        mods: Mods {
            ctrl,
            alt: false,
            shift: false,
        },
    };
    let hostile = Keymap {
        binds: vec![(
            KeyAction::ToggleTools,
            vec![
                spec(Key::Char('y'), false),
                spec(Key::Char('n'), false),
                spec(Key::Esc, false),
                spec(Key::Char('c'), true),
                spec(Key::Enter, false),
                spec(Key::Backspace, false),
                spec(Key::Char('x'), false),
                spec(Key::Tab, false),
            ],
        )],
    };
    for p in [
        press(Key::Char('y'), false, false, false),
        press(Key::Char('n'), false, false, false),
        press(Key::Esc, false, false, false),
        press(Key::Char('c'), true, false, false),
        press(Key::Enter, false, false, false),
        press(Key::Backspace, false, false, false),
        press(Key::Char('x'), false, false, false),
        press(Key::Tab, false, false, false),
    ] {
        assert_eq!(hostile.action(&p), None, "{p:?}");
    }
    // The approval prompt itself, under that table.
    let mut app = crate::App::default().with_keymap(hostile);
    let (tx, rx) = std::sync::mpsc::channel();
    let pend = |tx: std::sync::mpsc::Sender<tole_core::approval::Verdict>| {
        crate::Pending::new(
            crate::ApprovalRequest::new("write_file a", tole_core::tool::Risk::Write),
            move |v| {
                let _ = tx.send(v);
            },
        )
    };
    app.on_msg(crate::Msg::Approval(pend(tx)));
    app.on_input(crate::Input::new(Key::Char('y')));
    assert_eq!(rx.try_recv(), Ok(tole_core::approval::Verdict::Allow));
}
