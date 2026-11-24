//! The window's source of the Automations page: what the page reads before and
//! after there is a serve, and that a press with no serve is refused. Real
//! threads; every wait is on a condition.

use std::sync::Arc;

use tole_desktop_model::{Load, ServeState};

use super::fake::{launcher_of, running, wait_until, Counts, FakeServe};
use super::App;
use crate::driver::SystemClock;
use crate::ui::automations::{AutomationsCall, AutomationsRefused};
use crate::ui::Source;

fn start(serve: &Arc<FakeServe>) -> App {
    App::start_with(launcher_of(serve.clone(), None), Arc::new(SystemClock)).expect("threads")
}

fn calls() -> [AutomationsCall; 3] {
    [
        AutomationsCall::SetEnabled {
            id: "j1".into(),
            enabled: true,
        },
        AutomationsCall::Select("j1".into()),
        AutomationsCall::DismissNotice,
    ]
}

#[test]
fn without_a_serve_the_page_is_not_read_and_a_press_is_refused() {
    let serve = FakeServe::new(ServeState::Starting, None);
    let app = start(&serve);
    let view = Source::automations(&app);
    assert_eq!(view.page.load, Load::Idle, "never read, never empty");
    assert!(view.page.rows.is_empty() && view.page.selected.is_none());
    for call in calls() {
        assert_eq!(app.automations_call(call), Err(AutomationsRefused::NoServe));
    }
    let _ = app.quit(Default::default());
}

#[test]
fn with_a_serve_a_list_that_cannot_be_read_says_so_and_a_press_is_taken() {
    // The scripted serve refuses the cron routes: the page must say it could
    // not read the list, not show an empty one.
    let counts = Counts {
        approvals: 0,
        drafts: 0,
        untrusted_tools: 0,
    };
    let serve = FakeServe::new(running(), Some(counts));
    let app = start(&serve);
    wait_until("the list is reported as not readable", || {
        matches!(Source::automations(&app).page.load, Load::Failed(_))
    });
    assert!(Source::automations(&app).page.rows.is_empty());
    for call in calls() {
        assert_eq!(app.automations_call(call), Ok(()), "a driver takes it");
    }
    let _ = app.quit(Default::default());
}
