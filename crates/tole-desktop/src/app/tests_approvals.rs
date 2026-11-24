//! The window's source of the Approvals page over a scripted serve: what the
//! page reads, and that an answer reaches the serve once, with the verdict of
//! the button that was pressed. Real threads; every wait is on a condition.

use std::sync::Arc;

use tole_client::Decision as Verdict;
use tole_desktop_model::approvals::{Decision, Tab};
use tole_desktop_model::{Badge, ServeState};

use super::fake::{launcher_of, running, wait_until, Counts, FakeServe};
use super::App;
use crate::driver::SystemClock;
use crate::ui::approvals::{ApprovalsCall, ApprovalsRefused};
use crate::ui::Source;

fn start(serve: &Arc<FakeServe>) -> App {
    App::start_with(launcher_of(serve.clone(), None), Arc::new(SystemClock)).expect("threads")
}

fn counts(approvals: usize) -> Option<Counts> {
    Some(Counts {
        approvals,
        drafts: 0,
        untrusted_tools: 0,
    })
}

fn decide(app: &App, id: &str, decision: Decision) -> Result<(), ApprovalsRefused> {
    app.approvals_call(ApprovalsCall::Decide {
        id: id.to_owned(),
        decision,
    })
}

#[test]
fn an_answer_reaches_the_serve_once_with_the_verdict_of_the_button_pressed() {
    let serve = FakeServe::new(running(), counts(2));
    let app = start(&serve);
    wait_until("both requests are listed", || {
        let view = Source::approvals(&app);
        view.badge == Badge::Count(2) && view.rows.len() == 2
    });
    let view = Source::approvals(&app);
    assert!(view.selected.as_ref().is_some_and(|c| c.answerable()));

    assert_eq!(decide(&app, "a0", Decision::AllowOnce), Ok(()));
    assert_eq!(decide(&app, "a1", Decision::DontAllow), Ok(()));
    wait_until("both answers reached the serve", || {
        serve.decided.lock().unwrap().len() == 2
    });
    let mut sent = serve.decided.lock().unwrap().clone();
    sent.sort_by(|a, b| a.0.cmp(&b.0));
    assert_eq!(
        sent,
        [
            ("a0".to_owned(), Verdict::Allow),
            ("a1".to_owned(), Verdict::Deny)
        ]
    );
    wait_until("nothing waits", || {
        Source::approvals(&app).badge == Badge::Count(0)
    });

    // A second press on an answered request is the model's to refuse: the call
    // is taken (the answer is asynchronous) but nothing more reaches the serve.
    // The driver takes its inputs in order, so once the tab has changed the
    // second press has been handled.
    assert_eq!(decide(&app, "a0", Decision::DontAllow), Ok(()));
    assert_eq!(
        app.approvals_call(ApprovalsCall::SelectTab(Tab::Decided)),
        Ok(())
    );
    wait_until("the Decided tab is shown", || {
        Source::approvals(&app).tab == Tab::Decided
    });
    assert_eq!(Source::approvals(&app).rows.len(), 2, "both are in Decided");
    assert_eq!(serve.decided.lock().unwrap().len(), 2);
    let _ = app.quit(Default::default());
}

#[test]
fn without_a_serve_the_page_reads_unknown_and_a_press_is_refused() {
    let serve = FakeServe::new(ServeState::Starting, None);
    let app = start(&serve);
    let view = Source::approvals(&app);
    assert_eq!(view.badge, Badge::Unknown);
    assert!(view.rows.is_empty() && view.selected.is_none());
    for call in [
        ApprovalsCall::Select(Some("a0".into())),
        ApprovalsCall::SelectTab(Tab::Decided),
        ApprovalsCall::Decide {
            id: "a0".into(),
            decision: Decision::AllowOnce,
        },
    ] {
        assert_eq!(app.approvals_call(call), Err(ApprovalsRefused::NoServe));
    }
    assert!(serve.decided.lock().unwrap().is_empty());
    let _ = app.quit(Default::default());
}
