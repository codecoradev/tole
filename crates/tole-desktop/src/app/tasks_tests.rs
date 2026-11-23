//! The window's tasks through the app: "New task" makes a session on the serve
//! and opens it, the window reads its view and its commands reach the driver,
//! and a try that cannot start is reported until the next one.

use std::sync::Arc;

use super::fake::{launcher_of, running, wait_until, Counts, FakeServe};
use super::App;
use crate::backend::TaskRefused;
use crate::driver::SystemClock;
use crate::ui::{Source, TaskCall};
use tole_desktop_model::strings::StartProblem;
use tole_desktop_model::{Badge, ServeState};

const COUNTS: Counts = Counts {
    approvals: 0,
    drafts: 0,
    untrusted_tools: 0,
};

/// The backend follows the serve with its drivers: the counts are known.
fn attached(app: &App) {
    wait_until("the drivers", || {
        app.view().approvals.badge == Badge::Count(0)
    });
}

fn app_on(state: ServeState) -> (Arc<FakeServe>, App) {
    let serve = FakeServe::new(state, Some(COUNTS));
    let app = App::start_with(launcher_of(serve.clone(), None), Arc::new(SystemClock))
        .expect("threads can start");
    (serve, app)
}

#[test]
fn a_new_task_opens_and_the_window_reads_it_and_calls_it_until_it_is_closed() {
    let (_serve, app) = app_on(running());
    attached(&app);
    assert!(app.open_tasks().is_empty());
    assert_eq!(
        app.task_call("s1", TaskCall::Toggle(0)),
        Err(TaskRefused::NotOpen)
    );

    app.new_task(Some("/w/Invoices"));
    wait_until("the task to open", || app.open_tasks() == ["s1"]);
    assert!(!app.new_task_failed());
    wait_until("its driver", || app.task_view("s1").is_some());

    // The list's bookkeeping and the owner's commands reach the driver.
    let viewport = TaskCall::Viewport {
        top: 10.0,
        height: 600.0,
        overscan: 100.0,
    };
    assert_eq!(app.task_call("s1", viewport), Ok(()));
    assert_eq!(
        app.task_call("s1", TaskCall::Heights(vec![(0, 40.0)])),
        Ok(())
    );
    assert_eq!(app.task_call("s1", TaskCall::Toggle(0)), Ok(()));
    // A command is queued, and the driver counts the ones it cannot act on
    // (nothing to stop, the history still being read): that is how the test
    // sees that both reached it.
    let ignored = |app: &App| app.task_view("s1").map_or(0, |v| v.ignored);
    let before = ignored(&app);
    assert_eq!(app.task_call("s1", TaskCall::Send("tidy".into())), Ok(()));
    assert_eq!(app.task_call("s1", TaskCall::Stop), Ok(()));
    wait_until("both commands to reach the driver", || {
        ignored(&app) == before + 2
    });

    app.close_task("s1");
    assert!(app.open_tasks().is_empty());
    assert!(app.task_view("s1").is_none());
    assert_eq!(
        app.task_call("s1", TaskCall::Stop),
        Err(TaskRefused::NotOpen)
    );
    let _ = app.quit(Default::default());
}

#[test]
fn a_try_that_cannot_start_is_reported_the_window_is_woken_and_the_next_try_clears_it() {
    let (serve, app) = app_on(ServeState::Starting);
    app.take_change();
    app.new_task(None);
    assert!(app.new_task_failed(), "the serve is not ready");
    assert_eq!(app.new_task_problem(), Some(StartProblem::NotReady));
    assert!(app.take_change(), "the window is told");
    assert!(app.open_tasks().is_empty());

    serve.set(running());
    attached(&app);
    app.new_task(None);
    assert!(!app.new_task_failed(), "cleared by the next try");
    wait_until("the task to open", || app.open_tasks() == ["s1"]);
    let _ = app.quit(Default::default());
}

#[test]
fn each_way_a_new_task_fails_is_reported_as_its_own_problem() {
    let (_serve, app) = app_on(running());
    attached(&app);
    // The cap: said with its number, and nothing was asked of the serve.
    for id in ["a", "b", "c"] {
        app.backend.open_task(id).expect("within the cap");
    }
    app.new_task(None);
    assert_eq!(
        app.new_task_problem(),
        Some(StartProblem::TooMany { max: 3 })
    );
    // A task closed frees a place; the next try is not a problem.
    app.close_task("c");
    app.new_task(None);
    assert_eq!(app.new_task_problem(), None);
    wait_until("the task to open", || app.open_tasks().len() == 3);
    let _ = app.quit(Default::default());
}
