use super::*;

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
