//! A stream the serve refuses (404 after a restart, `closed`, 429): the reader
//! parks its session, probes it slowly and takes it back when it opens, end to
//! end through the threads.

use std::io;
use std::sync::mpsc::Sender;
use std::sync::Arc;

use tole_client::{Event, StreamItem};
use tole_desktop_model::approvals::ComposerState;
use tole_desktop_model::Badge;

use super::engine_tests::{io_err, status};
use super::rig::{counted, spare, Rig};
use crate::driver::clock::Clock;
use crate::driver::fake::{FakeApi, Frame};

/// A rig following `session`, whose first `fails` streams are refused with
/// `fail`; every later stream is queued by the test.
fn refused_rig(session: &str, fails: Vec<Frame>) -> (Arc<FakeApi>, Rig) {
    let api = FakeApi::new();
    spare(&api, vec![]);
    for f in fails {
        api.open_stream(session).send(f).unwrap();
    }
    let rig = Rig::start(api.clone(), &[session]);
    (api, rig)
}

/// `n` streams the serve answers with `code`.
fn refusals(n: usize, code: u16) -> Vec<Frame> {
    (0..n).map(|_| Frame::Fail(status(code))).collect()
}

fn parked(rig: &Rig) -> Vec<String> {
    rig.handle.snapshot().parked_sessions
}

#[test]
fn a_serve_restart_parks_the_session_and_the_resumed_session_rejoins() {
    // The serve restarted: its streams answer 404 until the task is resumed.
    let api = FakeApi::new();
    spare(&api, vec![]);
    for f in refusals(3, 404) {
        api.open_stream("s1").send(f).unwrap();
    }
    // The stream a probe finds once the owner has resumed the task.
    let reopened: Sender<Frame> = api.open_stream("s1");
    let rig = Rig::start(api.clone(), &["s1"]);
    rig.tick_until("known without the session, which is parked", |v| {
        counted(v, 0) && v.parked_sessions == vec!["s1".to_owned()]
    });
    assert_eq!(rig.handle.composer("s2"), ComposerState::Open);
    // The task is resumed: the stream says something, and the session is back
    // in the coverage, no longer parked.
    reopened.send(Frame::Item(StreamItem::Heartbeat)).unwrap();
    rig.tick_until_opens(&api, "s1", 4);
    rig.tick_until("rejoined", |v| {
        v.parked_sessions.is_empty() && counted(v, 0)
    });
    // And it holds the list again: its stream failing makes it unknown.
    reopened
        .send(Frame::Fail(io_err(io::ErrorKind::ConnectionReset)))
        .unwrap();
    rig.until("down again", |v| v.badge == Badge::Unknown);
}

#[test]
fn a_session_closed_for_good_stays_parked_and_costs_one_probe_per_interval() {
    let (api, rig) = refused_rig("s1", refusals(12, 404));
    rig.tick_until("parked", |v| !v.parked_sessions.is_empty());
    let parked_at = rig.clock.monotonic_ms();
    assert_eq!(api.opens("s1"), 3, "three refusals park it");
    // The probe wait is 30 s to 40 s from the third refusal, which came after
    // 1 s and 2 s of backoff: no probe before 30 s on the clock...
    while rig.clock.monotonic_ms() < 30_000 {
        assert!(rig.clock.wait_sleepers(2, super::rig::PATIENCE));
        rig.tick();
    }
    rig.sync();
    assert_eq!(api.opens("s1"), 3, "no probe inside the interval");
    // ...and one by 40 s after the parking at the latest.
    while api.opens("s1") < 4 {
        assert!(
            rig.clock.monotonic_ms() <= parked_at + 41_000,
            "a probe is due"
        );
        assert!(rig.clock.wait_sleepers(2, super::rig::PATIENCE));
        rig.tick();
    }
    rig.sync();
    assert_eq!(parked(&rig), vec!["s1".to_owned()], "still parked");
    assert!(counted(&rig.handle.snapshot(), 0), "the list stays known");
    // The next probe is another interval away.
    let probed_at = rig.clock.monotonic_ms();
    while rig.clock.monotonic_ms() < probed_at + 29_000 {
        assert!(rig.clock.wait_sleepers(2, super::rig::PATIENCE));
        rig.tick();
    }
    rig.sync();
    assert_eq!(api.opens("s1"), 4, "one probe per interval, not more");
}

#[test]
fn refusals_must_be_consecutive_to_park() {
    // 404, a network error, 404, 404: never three in a row.
    let fails = vec![
        Frame::Fail(status(404)),
        Frame::Fail(io_err(io::ErrorKind::ConnectionReset)),
        Frame::Fail(status(404)),
        Frame::Fail(status(404)),
        Frame::Fail(status(404)),
    ];
    let (api, rig) = refused_rig("s1", fails);
    rig.tick_until_opens(&api, "s1", 4);
    // The fourth attempt is over once the reader sleeps again.
    assert!(rig.clock.wait_sleepers(2, super::rig::PATIENCE));
    rig.sync();
    assert!(parked(&rig).is_empty(), "two in a row are not three");
    // The fifth refusal is the third in a row.
    rig.tick_until("parked at the third in a row", |v| {
        v.parked_sessions == vec!["s1".to_owned()]
    });
    assert_eq!(api.opens("s1"), 5);
}

#[test]
fn a_closed_frame_counts_as_a_refusal() {
    let closed = || Frame::Item(StreamItem::Event(Event::Closed { id: "s1".into() }));
    let fails = vec![closed(), Frame::Fail(status(404)), Frame::Fail(status(404))];
    let (_api, rig) = refused_rig("s1", fails);
    rig.tick_until("parked after closed and two 404s", |v| {
        v.parked_sessions == vec!["s1".to_owned()]
    });
}

#[test]
fn a_stream_the_serve_keeps_refusing_with_429_does_not_hold_the_app_unknown() {
    let (_api, rig) = refused_rig("s1", refusals(3, 429));
    let v = rig.tick_until("known, the session degraded", |v| {
        counted(v, 0) && v.parked_sessions == vec!["s1".to_owned()]
    });
    assert_eq!(v.fetch_failures, 0);
    assert_eq!(rig.handle.composer("s2"), ComposerState::Open);
}

#[test]
fn the_same_session_twice_is_followed_once() {
    let api = FakeApi::new();
    spare(&api, vec![]);
    let stream = api.open_stream("s1");
    let other = api.open_stream("s1");
    let rig = Rig::start(api.clone(), &["s1", "s1"]);
    stream.send(Frame::Item(StreamItem::Heartbeat)).unwrap();
    rig.tick_until("known", |v| counted(v, 0));
    assert_eq!(api.opens("s1"), 1, "one reader, one stream slot");
    // Its stream ending and returning is a loss and a recovery, as for one.
    drop(stream);
    rig.until("lost", |v| v.badge == Badge::Unknown);
    rig.tick_until_opens(&api, "s1", 2);
    other.send(Frame::Item(StreamItem::Heartbeat)).unwrap();
    rig.tick_until("known again", |v| counted(v, 0));
}
