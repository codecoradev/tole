//! The backend on its threads, over a scripted link and a clock the test moves:
//! the drivers follow the serve.

use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

use tole_desktop_model::{ServeFailure, ServeState};

use super::fake::{restarting, running, FakeLink};
use super::rig::{is_known, is_unknown, known_api, Rig};
use super::ServeLink;

fn secs(n: u64) -> Duration {
    Duration::from_secs(n)
}

#[test]
fn a_ready_serve_gets_the_drivers_and_the_view_shows_what_they_know() {
    let link = FakeLink::ready();
    link.queue_api(known_api(2, 1));
    let rig = Rig::start(link.clone());
    let v = rig.until("known", |v| is_known(v, 2, 1));
    assert_eq!(v.serve, running("1"));
    assert!(v.open_tasks.is_empty());
    assert_eq!(link.api_calls(), 1);
    assert!(rig.backend().is_running());
}

#[test]
fn before_the_serve_is_ready_the_view_reads_unknown_and_nothing_is_asked_of_it() {
    let link = FakeLink::new(ServeState::Starting);
    let rig = Rig::start(link.clone());
    rig.advance(secs(5));
    let v = rig.view();
    assert_eq!(v.serve, ServeState::Starting);
    assert!(is_unknown(&v));
    assert_eq!(link.api_calls(), 0, "no api is asked for");
    assert!(rig.backend().with_approvals(|_| ()).is_none());
    assert!(rig.backend().with_skills(|_| ()).is_none());
}

#[test]
fn a_serve_that_becomes_ready_gets_its_drivers() {
    let link = FakeLink::new(ServeState::Starting);
    link.queue_api(known_api(0, 0));
    let rig = Rig::start(link.clone());
    link.set(running("1"));
    rig.until_ticking("known", |v| is_known(v, 0, 0));
}

#[test]
fn leaving_the_ready_state_reads_unknown_before_the_worker_has_looked() {
    let link = FakeLink::ready();
    link.queue_api(known_api(1, 1));
    let rig = Rig::start(link.clone());
    rig.until("known", |v| is_known(v, 1, 1));
    // The clock stands still: the worker has not had a tick to notice.
    link.set(restarting());
    let v = rig.view();
    assert_eq!(v.serve, restarting());
    assert!(is_unknown(&v), "never the old serve's numbers: {v:?}");
    assert!(rig.backend().with_approvals(|_| ()).is_none());
    assert!(rig.backend().with_skills(|_| ()).is_none());
}

#[test]
fn the_drivers_are_stopped_when_the_serve_stops_being_ready() {
    let version_mismatch = ServeFailure::VersionMismatch {
        found: Some(1),
        expected: 2,
    };
    for gone in [
        restarting(),
        ServeState::Stopping,
        ServeState::Stopped,
        ServeState::Failed(version_mismatch),
    ] {
        let link = FakeLink::ready();
        link.queue_api(known_api(1, 0));
        let rig = Rig::start(link.clone());
        rig.until("known", |v| is_known(v, 1, 0));
        link.set(gone.clone());
        // Four approvals threads and four skills threads, joined.
        rig.wait_ticking("the drivers were stopped", |b| {
            b.thread_report().joined >= 8
        });
        let v = rig.until("unknown", is_unknown);
        assert_eq!(v.serve, gone);
        assert_eq!(rig.backend().thread_report().detached, 0);
    }
}

#[test]
fn a_serve_that_is_ready_again_gets_new_drivers_over_a_new_api() {
    let link = FakeLink::ready();
    let first = known_api(1, 0);
    let second = known_api(3, 2);
    link.queue_api(first.clone());
    link.queue_api(second.clone());
    let rig = Rig::start(link.clone());
    rig.until("known", |v| is_known(v, 1, 0));
    link.set(restarting());
    rig.until_ticking("unknown", is_unknown);
    link.set(running("2"));
    let v = rig.until_ticking("known again", |v| is_known(v, 3, 2));
    assert_eq!(v.serve, running("2"));
    assert_eq!(link.api_calls(), 2, "one api per attach");
    assert!(Arc::ptr_eq(&link.given()[1], &second));
    assert_eq!(first.skills_calls(), 1, "the old drivers asked no more");
}

#[test]
fn a_restart_that_came_back_between_two_looks_still_gets_new_drivers() {
    let link = FakeLink::ready();
    link.queue_api(known_api(1, 0));
    link.queue_api(known_api(4, 0));
    let rig = Rig::start(link.clone());
    rig.until("known", |v| is_known(v, 1, 0));
    // Not ready and ready again before the worker looks: only the epoch shows it.
    link.set(restarting());
    link.set(running("2"));
    assert!(is_unknown(&rig.view()), "the old drivers are not shown");
    // The detach, then a gap, then the attach.
    rig.until_ticking("the new drivers", |v| is_known(v, 4, 0));
    assert_eq!(link.api_calls(), 2);
}

#[test]
fn a_serve_that_flaps_gets_drivers_at_a_bounded_pace() {
    let link = FakeLink::ready();
    let rig = Rig::start(link.clone());
    for i in 0..60 {
        link.set(if i % 2 == 0 {
            restarting()
        } else {
            running("1")
        });
        rig.tick();
    }
    // Six seconds of the clock: the first attach and one a second at most.
    assert!(link.api_calls() <= 7, "{} attaches", link.api_calls());
    link.set(running("1"));
    rig.wait_ticking("the drivers run", |b| {
        b.with_approvals(|h| h.is_running()) == Some(true)
    });
}

#[test]
fn an_api_that_cannot_be_built_is_retried_with_a_growing_wait() {
    let link = FakeLink::ready();
    link.refuse_api(3);
    link.queue_api(known_api(0, 0));
    let rig = Rig::start(link.clone());
    link.wait_api_calls(1);
    assert!(is_unknown(&rig.view()));
    // 1 s, then 2 s, then 4 s: the fourth call is the first that works.
    rig.advance(Duration::from_millis(700));
    assert_eq!(link.api_calls(), 1, "not before 1 s");
    for n in 2..=4 {
        rig.wait_ticking("another try", |_| link.api_calls() >= n);
    }
    rig.until_ticking("known", |v| is_known(v, 0, 0));
    assert_eq!(link.api_calls(), 4, "no more than needed");
}

#[test]
fn shutdown_stops_every_thread_and_leaves_the_serve_alone() {
    let link = FakeLink::ready();
    link.queue_api(known_api(0, 0));
    let mut rig = Rig::start(link.clone());
    rig.until("known", |v| is_known(v, 0, 0));
    let backend = rig.backend.take().expect("running");
    let report = backend.shutdown(secs(5));
    // The worker and the ticker, the approvals driver's four threads and the
    // skills driver's four.
    assert_eq!((report.panicked, report.detached), (0, 0), "{report:?}");
    assert_eq!(report.joined, 10, "{report:?}");
    assert_eq!(link.state(), running("1"), "the serve is not touched");
}

#[test]
fn dropping_the_backend_stops_it_and_tells_the_ui_once_more() {
    let link = FakeLink::ready();
    link.queue_api(known_api(0, 0));
    let mut rig = Rig::start(link.clone());
    rig.until("known", |v| is_known(v, 0, 0));
    let before = rig.changes.load(Ordering::SeqCst);
    drop(rig.backend.take());
    assert!(rig.changes.load(Ordering::SeqCst) > before);
}

#[test]
fn the_view_after_shutdown_is_unknown_and_nothing_is_running() {
    let link = FakeLink::ready();
    link.queue_api(known_api(0, 0));
    let rig = Rig::start(link.clone());
    rig.until("known", |v| is_known(v, 0, 0));
    let handle = rig
        .backend()
        .with_approvals(Clone::clone)
        .expect("a driver is there");
    let mut rig = rig;
    let backend = rig.backend.take().expect("running");
    assert!(backend.is_running());
    let report = backend.shutdown(secs(5));
    assert_eq!(report.detached, 0);
    assert!(
        !handle.is_running(),
        "the driver's own handle reads stopped"
    );
    assert!(handle.snapshot().badge == tole_desktop_model::Badge::Unknown);
}

#[test]
fn a_link_that_panics_reads_as_not_ready_and_the_backend_recovers() {
    let link = FakeLink::ready();
    link.queue_api(known_api(1, 0));
    link.queue_api(known_api(2, 0));
    let rig = Rig::start(link.clone());
    rig.until("known", |v| is_known(v, 1, 0));
    link.panic_on_state(true);
    // A snapshot on the UI's thread does not panic with it.
    let v = rig.view();
    assert_eq!(v.serve, ServeState::Starting);
    assert!(is_unknown(&v));
    rig.wait_ticking("the drivers were stopped", |b| {
        b.thread_report().joined >= 8
    });
    assert!(rig.backend().is_running(), "the worker survived");
    link.panic_on_state(false);
    rig.until_ticking("known again", |v| is_known(v, 2, 0));
}

#[test]
fn a_link_whose_api_panics_is_a_failed_attach_and_not_a_dead_backend() {
    let link = FakeLink::ready();
    link.panic_on_api(true);
    link.queue_api(known_api(0, 0));
    let rig = Rig::start(link.clone());
    link.wait_api_calls(1);
    assert!(is_unknown(&rig.view()));
    assert!(rig.backend().is_running());
    link.panic_on_api(false);
    rig.until_ticking("known", |v| is_known(v, 0, 0));
}

fn failed(detail: &str) -> ServeState {
    ServeState::Failed(ServeFailure::StartFailed {
        detail: detail.into(),
    })
}

/// Set the serve to `state` and wait until the UI has been told (the clock
/// moves a tick at a time, as the UI's callback is paced by it); the view then
/// shows that state.
fn told_of(rig: &Rig, link: &FakeLink, state: ServeState) {
    let before = rig.changes.load(Ordering::SeqCst);
    link.set(state.clone());
    rig.wait_ticking("the UI was told", |_| {
        rig.changes.load(Ordering::SeqCst) > before
    });
    assert_eq!(rig.view().serve, state);
}

#[test]
fn the_ui_is_told_of_a_failed_serve_that_is_retried_though_no_driver_exists() {
    // Failed, the owner presses Retry: Starting, then Failed again.
    let link = FakeLink::new(failed("a"));
    let rig = Rig::start(link.clone());
    // The worker's first look only records the state (the UI reads the view
    // when it starts); the second shows the first was done.
    rig.wait_ticking("the worker has looked twice", |_| link.looks() >= 2);
    told_of(&rig, &link, ServeState::Starting);
    told_of(&rig, &link, failed("b"));
    told_of(&rig, &link, ServeState::Starting);
    assert_eq!(link.api_calls(), 0, "no driver was ever asked for");
    // Nothing changed, nothing is said, however long it takes.
    let quiet = rig.changes.load(Ordering::SeqCst);
    rig.advance(secs(3));
    assert_eq!(rig.changes.load(Ordering::SeqCst), quiet);
}

#[test]
fn the_ui_is_told_of_each_step_of_a_restart_that_goes_on_after_the_detach() {
    let link = FakeLink::ready();
    link.queue_api(known_api(0, 0));
    let rig = Rig::start(link.clone());
    rig.until("known", |v| is_known(v, 0, 0));
    // The detach is the first step; the later ones have no driver to say them.
    told_of(&rig, &link, restarting());
    let second = ServeState::Restarting {
        attempt: 2,
        retry_in: secs(2),
    };
    told_of(&rig, &link, second);
    let gave_up = ServeState::Failed(ServeFailure::GaveUp {
        attempts: 5,
        detail: "d".into(),
    });
    told_of(&rig, &link, gave_up);
    assert!(is_unknown(&rig.view()));
    assert_eq!(link.api_calls(), 1);
}
