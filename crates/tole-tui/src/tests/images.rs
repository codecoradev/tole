use super::*;

#[test]
fn slash_image_maps_to_the_three_image_commands() {
    for (draft, want) in [
        ("/image shot.png", HostCmd::AttachImage("shot.png".into())),
        ("/image  clear ", HostCmd::ClearImages),
        ("/image", HostCmd::Images),
    ] {
        let mut app = App::default();
        typed_input(&mut app, draft);
        assert_eq!(
            app.on_input(Input::new(Key::Enter)),
            Action::Host(want),
            "{draft}"
        );
    }
    let (notes, _, _) = host_says(
        HostCmd::AttachImage("/etc/x.png".into()),
        Err("image: /etc/x.png: outside the workspace jail — refused".into()),
    );
    assert_eq!(
        notes,
        vec!["(/image: image: /etc/x.png: outside the workspace jail — refused)"]
    );
    let host = FakeHost::answering(vec![Ok(HostReply::Images(vec![]))]);
    let mut app = App::default();
    app.set_images(vec![item("a.png"), item("b.png")]);
    run_host_cmd(&host, &mut app, HostCmd::ClearImages);
    assert!(app.images().is_empty());
    assert_eq!(texts(&mut app), vec!["(2 queued image(s) dropped)"]);
    let (notes, _, app) = host_says(HostCmd::Images, Ok(HostReply::Images(vec![item("a.png")])));
    assert_eq!(
        notes,
        vec![
            "(1 image(s) queued for your next message)",
            "  a.png (image/png, 1000 B)"
        ]
    );
    assert_eq!(app.images().len(), 1);
}

fn submit(draft: &str) -> Action {
    let mut app = App::default();
    typed_input(&mut app, draft);
    app.on_input(Input::new(Key::Enter))
}

#[test]
fn copy_and_export_map_to_host_commands() {
    assert_eq!(submit("/copy"), Action::Host(HostCmd::LastAnswer));
    assert_eq!(submit("/export"), Action::Host(HostCmd::Export(None)));
    assert_eq!(
        submit("/export  out/my run.html "),
        Action::Host(HostCmd::Export(Some("out/my run.html".into())))
    );
    // `/copy` takes no argument: refused locally, nothing sent.
    let mut app = App::default();
    typed_input(&mut app, "/copy now");
    assert_eq!(app.on_input(Input::new(Key::Enter)), Action::Redraw);
    assert!(texts(&mut app)[0].contains("usage: /copy"));
    assert!(menu::matching("/co").contains(&"/copy"));
    assert!(menu::matching("/ex").contains(&"/export"));
}

#[test]
fn copy_notes_what_was_attempted_and_queues_only_the_osc52_sequence() {
    let answer = "hi \u{e9}\x1b[2J\x07 ]52; end".to_string();
    let (notes, _, mut app) = host_says(
        HostCmd::LastAnswer,
        Ok(HostReply::Answer(Some(answer.clone()))),
    );
    assert_eq!(notes.len(), 1);
    let n = answer.chars().count();
    assert!(notes[0].contains(&format!("sent {n} characters to the terminal clipboard")));
    assert!(notes[0].contains("OSC 52") && notes[0].contains("set-clipboard on"));
    assert!(
        !notes[0].contains("copied"),
        "never claims success: {}",
        notes[0]
    );
    assert_eq!(app.take_osc52(), Some(clipboard::osc52(&answer)));
    assert_eq!(app.take_osc52(), None, "drains");
}

#[test]
fn copy_with_no_answer_or_too_large_an_answer_sends_nothing() {
    for reply in [None, Some("  \n".to_string())] {
        let (notes, _, mut app) = host_says(HostCmd::LastAnswer, Ok(HostReply::Answer(reply)));
        assert!(notes[0].contains("nothing to copy"), "{notes:?}");
        assert_eq!(app.take_osc52(), None);
    }
    let at_cap = "a".repeat(clipboard::MAX_COPY_BYTES);
    let (_, _, mut app) = host_says(HostCmd::LastAnswer, Ok(HostReply::Answer(Some(at_cap))));
    assert!(app.take_osc52().is_some(), "exactly the cap is allowed");
    let over = "a".repeat(clipboard::MAX_COPY_BYTES + 1);
    let (notes, _, mut app) = host_says(HostCmd::LastAnswer, Ok(HostReply::Answer(Some(over))));
    assert_eq!(app.take_osc52(), None);
    assert!(
        notes[0].contains("/export") && notes[0].contains("64 KiB"),
        "{notes:?}"
    );
    let (notes, _, mut app) = host_says(HostCmd::LastAnswer, Err("boom".into()));
    assert_eq!(notes, vec!["(/copy: boom)"]);
    assert_eq!(app.take_osc52(), None);
}

#[test]
fn export_reports_the_written_path_or_the_hosts_refusal() {
    let (notes, _, _) = host_says(
        HostCmd::Export(None),
        Ok(HostReply::Path("/w/.tole/exports/s-1.html".into())),
    );
    assert_eq!(
        notes,
        vec!["(transcript written to /w/.tole/exports/s-1.html)"]
    );
    let (notes, _, _) = host_says(
        HostCmd::Export(Some("a.html".into())),
        Err("a.html exists; pick another path".into()),
    );
    assert_eq!(notes, vec!["(/export: a.html exists; pick another path)"]);
    // A wrong-kind reply is reported, never a panic.
    let (notes, _, _) = host_says(HostCmd::Export(None), Ok(HostReply::Done));
    assert!(notes[0].contains("unexpected host reply"));
}

#[test]
fn queued_images_leave_with_the_message_and_are_reread_after_the_turn() {
    let mut app = App::default();
    app.set_images(vec![item("a.png"), item("b.png")]);
    assert_eq!(
        app.pending_rows(200),
        vec!["images (2): a.png (image/png, 1000 B), b.png (image/png, 1000 B) · next message"]
    );
    typed_input(&mut app, "look");
    app.on_input(Input::new(Key::Enter));
    assert!(
        app.images().is_empty(),
        "the host took them with the message"
    );
    assert!(app.pending_rows(200).is_empty());
    app.on_msg(Msg::Done(TurnEnd::Stopped("provider failed".into())));
    assert!(
        std::mem::take(&mut app.images_stale),
        "the loop asks the host what is queued now"
    );
    // A turn without images does not ask.
    typed_input(&mut app, "plain");
    app.on_input(Input::new(Key::Enter));
    app.on_msg(Msg::Done(TurnEnd::Final("ok".into())));
    assert!(!app.images_stale);
}
