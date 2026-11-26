use super::*;

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
