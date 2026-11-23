//! The app over a scripted serve: the view, the wake-up, a launch that fails,
//! a serve that flaps, and the commands. Real threads and the system clock;
//! every wait is on a condition and bounded only so a bug fails a test.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use tole_desktop_model::{
    Badge, ConnectionStatus, OwnerAction, ServeFailure, ServeState, ShellState,
};

use super::fake::{launcher_of, running, wait_until, Counts, FakeServe, Gate};
use super::launch::{Launcher, Serve};
use super::signal::Signal;
use super::App;
use crate::backend::shell;
use crate::driver::SystemClock;

const COUNTS: Counts = Counts {
    approvals: 2,
    drafts: 2,
    untrusted_tools: 1,
};

fn start(launcher: Launcher) -> App {
    App::start_with(launcher, Arc::new(SystemClock)).expect("threads can start")
}

fn restarting() -> ServeState {
    ServeState::Restarting {
        attempt: 1,
        retry_in: Duration::from_secs(1),
    }
}

/// Close the app: nothing the backend started may have panicked.
fn finish(app: App) {
    let quit = app.quit(Default::default());
    assert_eq!(quit.backend.panicked, 0, "{quit:?}");
}

/// The two rules of the view, which must hold at every look, whatever the
/// serve does.
fn assert_never_a_zero(app: &App) {
    let view = app.view();
    if !view.serve.is_ready() {
        assert_eq!(view.approvals.badge, Badge::Unknown, "{:?}", view.serve);
        assert_eq!(
            view.skills.skills_waiting,
            Badge::Unknown,
            "{:?}",
            view.serve
        );
    }
}

#[test]
fn the_window_reads_starting_until_the_launch_settles_and_is_told_when_it_does() {
    let serve = FakeServe::new(running(), Some(COUNTS));
    let gate = Gate::new();
    let app = start(launcher_of(serve, Some(gate.clone())));

    // The launch blocks, and the window is not: it has a state to show.
    wait_until("the launch to begin", || gate.arrivals() == 1);
    assert_eq!(app.view().serve, ServeState::Starting);
    assert!(app.is_running());
    app.take_change(); // a change that came before the look
    assert!(!app.take_change());

    gate.open();
    wait_until("the serve to be running", || app.view().serve == running());
    assert!(
        app.take_change(),
        "the window is woken when the launch ends"
    );
    finish(app);
}

#[test]
fn the_counts_reach_the_shell_and_unknown_is_never_written_as_zero() {
    let serve = FakeServe::new(running(), Some(COUNTS));
    let gate = Gate::new();
    let app = start(launcher_of(serve, Some(gate.clone())));
    let mut shell_state = ShellState::new();

    wait_until("the launch to begin", || gate.arrivals() == 1);
    let applied = shell::apply(&mut shell_state, &app.view());
    assert_eq!(shell_state.connection.status, ConnectionStatus::Connecting);
    assert_eq!(shell_state.approvals_pending, Badge::Unknown);
    assert!(applied.skills_unknown, "the skills count is not known yet");

    gate.open();
    wait_until("the approvals to be counted", || {
        app.view().approvals.badge == Badge::Count(2)
    });
    wait_until("the skills and tools to be counted", || {
        shell::skills_badge(&app.view()) == Badge::Count(3)
    });
    let applied = shell::apply(&mut shell_state, &app.view());
    assert!(!applied.skills_unknown);
    assert_eq!(shell_state.connection.status, ConnectionStatus::Connected);
    assert_eq!(shell_state.approvals_pending, Badge::Count(2));
    assert_eq!(shell_state.skills_new, 3);
    finish(app);
}

#[test]
fn a_serve_that_stops_being_ready_reads_unknown_at_once() {
    let serve = FakeServe::new(running(), Some(COUNTS));
    let app = start(launcher_of(serve.clone(), None));
    wait_until("the counts", || {
        app.view().approvals.badge == Badge::Count(2)
            && app.view().skills.tools_needing_attention == Badge::Count(1)
    });

    serve.set(restarting());
    // No wait for the backend's worker: the snapshot reads the serve itself.
    let view = app.view();
    assert_eq!(view.serve, restarting());
    assert_eq!(view.approvals.badge, Badge::Unknown);
    assert_eq!(view.skills.skills_waiting, Badge::Unknown);
    wait_until("a wake-up", || app.take_change());
    finish(app);
}

#[test]
fn a_flapping_serve_never_shows_a_count_it_does_not_know() {
    let serve = FakeServe::new(running(), Some(COUNTS));
    let app = start(launcher_of(serve.clone(), None));
    let states = [restarting(), running(), ServeState::Starting, running()];
    for round in 0..400 {
        serve.set(states[round % states.len()].clone());
        assert_never_a_zero(&app);
        if round % 7 == 0 {
            std::thread::sleep(Duration::from_millis(3));
        }
    }
    serve.set(running());
    wait_until("the counts after the flapping", || {
        app.view().approvals.badge == Badge::Count(2)
    });
    assert!(app.is_running());
    let quit = app.quit(Default::default());
    assert_eq!(quit.backend.panicked, 0);
}

#[test]
fn a_launch_that_panics_is_a_state_the_owner_can_read() {
    let serve = FakeServe::new(running(), Some(COUNTS));
    let attempts = Arc::new(AtomicUsize::new(0));
    let launcher: Launcher = {
        let (attempts, serve) = (attempts.clone(), serve.clone());
        Arc::new(move || {
            if attempts.fetch_add(1, Ordering::SeqCst) == 0 {
                panic!("the disk is gone");
            }
            let serve: Arc<dyn Serve> = serve.clone();
            serve
        })
    };
    let app = start(launcher);

    wait_until("the failed launch to show", || {
        matches!(
            app.view().serve,
            ServeState::Failed(ServeFailure::StartFailed { .. })
        )
    });
    let view = app.view();
    let ServeState::Failed(ServeFailure::StartFailed { detail }) = &view.serve else {
        panic!("a failed launch is a failed state: {:?}", view.serve);
    };
    assert!(detail.contains("the disk is gone"), "{detail}");
    assert_eq!(view.serve.owner_action(), Some(OwnerAction::Retry));
    assert_eq!(view.approvals.badge, Badge::Unknown);
    assert!(
        app.is_running(),
        "a launch that failed does not end the app"
    );

    finish(app);
}

#[test]
fn a_backend_that_cannot_start_leaves_no_serve_behind() {
    let launched = Arc::new(AtomicUsize::new(0));
    let launcher: Launcher = {
        let launched = launched.clone();
        Arc::new(move || {
            launched.fetch_add(1, Ordering::SeqCst);
            panic!("this launch must never begin");
        })
    };
    let result = App::assemble(launcher, Signal::new(), |_link| {
        Err(std::io::Error::other("no threads"))
    });
    assert!(result.is_err());
    std::thread::sleep(Duration::from_millis(100));
    assert_eq!(launched.load(Ordering::SeqCst), 0, "a launch began");
}
