//! Closing the window: a launch still running, a serve that is slow to close,
//! a launch that failed. Each must end within its bound.

use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, Instant};

use tole_desktop_model::{Badge, ServeFailure, ServeState};

use super::fake::{launcher_of, running, wait_until, Counts, FakeServe, Gate};
use super::launch::{CloseOutcome, Launcher, Serve};
use super::{App, Limits, ServeQuit};
use crate::driver::SystemClock;

const COUNTS: Counts = Counts {
    approvals: 1,
    drafts: 0,
    untrusted_tools: 0,
};

fn start(launcher: Launcher) -> App {
    App::start_with(launcher, Arc::new(SystemClock)).expect("threads can start")
}

fn quick() -> Limits {
    Limits {
        backend: Duration::from_millis(1500),
        serve: Duration::from_millis(300),
    }
}

/// For a close that must be seen to end, however slow a loaded machine is.
fn patient() -> Limits {
    Limits {
        backend: Duration::from_millis(1500),
        serve: Duration::from_secs(20),
    }
}

#[test]
fn closing_while_the_launch_runs_does_not_wait_for_it_and_closes_its_serve_later() {
    let serve = FakeServe::new(running(), Some(COUNTS));
    let gate = Gate::new();
    let app = start(launcher_of(serve.clone(), Some(gate.clone())));
    wait_until("the launch to begin", || gate.arrivals() == 1);

    let began = Instant::now();
    let quit = app.quit(quick());
    // The launch is blocked for as long as the supervisor's start-up allows
    // (tens of seconds): the quit takes the backend's stop, not that.
    assert!(
        began.elapsed() < Duration::from_secs(5),
        "{:?}",
        began.elapsed()
    );
    assert_eq!(quit.serve, ServeQuit::LaunchLetGo);
    assert_eq!(quit.backend.panicked, 0);
    assert_eq!(
        serve.closes.load(Ordering::SeqCst),
        0,
        "nothing to close yet"
    );

    // The launch ends after the window is gone: its serve is closed by the
    // launch thread and not kept.
    gate.open();
    wait_until("the late serve to be closed", || {
        serve.closes.load(Ordering::SeqCst) == 1
    });
    wait_until("the late serve to be released", || {
        Arc::strong_count(&serve) == 1
    });
}

#[test]
fn closing_a_running_app_closes_the_window_of_the_serve_once() {
    let serve = FakeServe::new(running(), Some(COUNTS));
    let app = start(launcher_of(serve.clone(), None));
    wait_until("the approvals", || {
        app.view().approvals.badge == Badge::Count(1)
    });

    let began = Instant::now();
    let quit = app.quit(Limits::default());
    assert_eq!(quit.serve, ServeQuit::Closed);
    assert_eq!(serve.closes.load(Ordering::SeqCst), 1);
    assert_eq!(quit.backend.panicked, 0);
    assert!(
        quit.backend.joined >= 2,
        "the worker and the ticker: {quit:?}"
    );
    assert!(began.elapsed() < Duration::from_secs(5));
}

#[test]
fn a_serve_that_is_slow_to_close_is_left_after_the_bound() {
    let serve = FakeServe::new(running(), Some(COUNTS));
    let hold = Gate::new();
    *serve.close_gate.lock().unwrap() = Some(hold.clone());
    let app = start(launcher_of(serve.clone(), None));
    wait_until("the serve", || app.view().serve == running());

    let began = Instant::now();
    let quit = app.quit(quick());
    assert_eq!(quit.serve, ServeQuit::Left);
    assert!(
        began.elapsed() < Duration::from_secs(5),
        "{:?}",
        began.elapsed()
    );
    assert_eq!(serve.closes.load(Ordering::SeqCst), 1);
    hold.open();
}

#[test]
fn closing_after_a_failed_launch_has_no_serve_to_close() {
    let launcher: Launcher = Arc::new(|| panic!("no serve today"));
    let app = start(launcher);
    wait_until("the failed launch", || {
        matches!(
            app.view().serve,
            ServeState::Failed(ServeFailure::StartFailed { .. })
        )
    });
    let quit = app.quit(quick());
    assert_eq!(quit.serve, ServeQuit::NoServe);
}

#[test]
fn dropping_the_app_without_quitting_stops_the_backend_and_leaves_the_serve() {
    let serve = FakeServe::new(running(), Some(COUNTS));
    let app = start(launcher_of(serve.clone(), None));
    wait_until("the serve", || app.view().serve == running());
    let began = Instant::now();
    drop(app);
    assert!(began.elapsed() < Duration::from_secs(5));
    assert_eq!(serve.closes.load(Ordering::SeqCst), 0);
    wait_until("the serve to be released", || {
        Arc::strong_count(&serve) == 1
    });
}

#[test]
fn a_serve_that_was_only_released_or_could_not_be_stopped_is_not_reported_as_closed() {
    for (outcome, expected) in [
        (CloseOutcome::Held, ServeQuit::Held),
        (CloseOutcome::Failed, ServeQuit::Failed),
    ] {
        let serve = FakeServe::new(running(), Some(COUNTS));
        *serve.close_outcome.lock().unwrap() = outcome;
        let app = start(launcher_of(serve.clone(), None));
        wait_until("the serve", || app.view().serve == running());
        assert_eq!(app.quit(patient()).serve, expected);
    }
}

#[test]
fn a_close_that_panics_is_reported_as_failed() {
    struct Panics(Arc<FakeServe>);
    impl crate::backend::ServeLink for Panics {
        fn state(&self) -> ServeState {
            self.0.state()
        }
        fn epoch(&self) -> u64 {
            self.0.epoch()
        }
        fn api(&self) -> Option<Arc<dyn crate::driver::Api>> {
            self.0.api()
        }
    }
    impl Serve for Panics {
        fn close_window(self: Arc<Self>) -> CloseOutcome {
            panic!("the supervisor broke");
        }
    }
    let inner = FakeServe::new(running(), None);
    let launcher: Launcher = Arc::new(move || {
        let serve: Arc<dyn Serve> = Arc::new(Panics(inner.clone()));
        serve
    });
    let app = start(launcher);
    wait_until("the serve", || app.view().serve == running());
    assert_eq!(app.quit(patient()).serve, ServeQuit::Failed);
}

/// The real supervisor, started on a binary that does not exist (so no serve
/// is ever started), and closed.
mod real_supervisor {
    use std::ffi::OsString;
    use std::os::unix::fs::PermissionsExt;
    use std::sync::Arc;

    use super::{CloseOutcome, Serve};
    use crate::backend::{ServeLink, SupervisorLink};
    use crate::supervisor::{CloseBehavior, Config};
    use tole_desktop_model::{ServeFailure, ServeState};

    fn launched(tag: &str, behavior: CloseBehavior) -> SupervisorLink {
        let dir = std::env::temp_dir().join(format!("tdquit-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
        let mut cfg = Config::new(&dir, dir.join("run"));
        cfg.binary = Some(dir.join("no-such-tole"));
        cfg.path_override = Some(OsString::new());
        cfg.close_behavior = behavior;
        SupervisorLink::launch(cfg)
    }

    #[test]
    fn closing_the_only_handle_closes_and_another_handle_means_held() {
        for behavior in [CloseBehavior::LeaveRunning, CloseBehavior::StopServe] {
            let link = Arc::new(launched("one", behavior));
            assert!(
                matches!(
                    link.state(),
                    ServeState::Failed(
                        ServeFailure::BinaryNotFound | ServeFailure::StartFailed { .. }
                    )
                ),
                "{:?}",
                link.state()
            );
            assert_eq!(link.clone().close_window(), CloseOutcome::Held);
            assert_eq!(link.close_window(), CloseOutcome::Closed, "{behavior:?}");
        }
    }
}
