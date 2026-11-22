//! Changing the followed sessions end to end: real threads, the real model, a
//! scripted [`FakeApi`] and a [`ManualClock`]. A stream nobody scripted stays
//! silent, so a reader inside it cannot end until the test sends a frame: the
//! same as a client that cannot cancel a read.

use std::sync::mpsc::Sender;
use std::sync::Arc;
use std::time::Duration;

use tole_client::{Event, StreamItem};
use tole_desktop_model::Badge;

use super::engine_tests::{list, row};
use super::rig::{counted, following, pending, spare, Rig, PATIENCE};
use super::store::Store;
use super::MAX_STREAMS;
use crate::driver::fake::{FakeApi, Frame};

fn names(v: &[&str]) -> Vec<String> {
    v.iter().map(|s| (*s).to_owned()).collect()
}

/// A rig following `sessions`, every stream up and the list known. The script
/// holds plenty of lists; the streams are returned in the order of the names.
fn known_rig(sessions: &[&str]) -> (Arc<FakeApi>, Rig, Vec<Sender<Frame>>) {
    let api = FakeApi::new();
    spare(&api, vec![]);
    let streams: Vec<_> = sessions.iter().map(|s| api.open_stream(s)).collect();
    let rig = Rig::start(api.clone(), sessions);
    for stream in &streams {
        Rig::up(stream);
    }
    rig.tick_until("known", |v| counted(v, 0));
    (api, rig, streams)
}

/// A frame that says a request of `session` is waiting.
fn requested(id: &str, session: &str) -> Frame {
    let event =
        Event::ApprovalRequested(serde_json::from_value(row(id, session, "pending")).unwrap());
    Frame::Item(StreamItem::Event(event))
}

#[test]
fn a_session_added_while_running_gets_a_reader_and_is_known_after_its_stream() {
    let (api, rig, _s1) = known_rig(&["s1"]);
    let s2 = api.open_stream("s2");
    let before = api.list_calls();
    rig.handle.set_sessions(names(&["s1", "s2"])).unwrap();
    api.wait_opens("s2", 1);
    // The fetch the change asks for begins with the new stream still down.
    rig.tick_until_calls(&api, before + 1);
    rig.until("not known for the new set", |v| v.badge == Badge::Unknown);
    Rig::up(&s2);
    rig.tick_until("known after the stream and a fetch", |v| counted(v, 0));
    assert_eq!(api.opens("s1"), 1, "the old reader is not restarted");
}

#[test]
fn a_queue_read_racing_a_new_stream_does_not_make_the_list_known() {
    let (api, rig, s1) = following(pending());
    let gate = api.gate_list();
    // A fetch is out (a reconnect asked for it) when the session is added.
    s1.send(Frame::Item(StreamItem::Reconnected { after: 7 }))
        .unwrap();
    rig.tick_until_calls(&api, 3);
    let s2 = api.open_stream("s2");
    rig.handle.set_sessions(names(&["s1", "s2"])).unwrap();
    api.wait_opens("s2", 1);
    Rig::up(&s2);
    rig.sync();
    // The reply comes with the new stream up, yet the read began before it.
    gate.send(Ok(list(pending()))).unwrap();
    rig.sync();
    assert_eq!(rig.handle.snapshot().badge, Badge::Unknown);
    spare(&api, pending());
    rig.tick_until("known by a read that began after the stream", |v| {
        counted(v, 1)
    });
    assert!(api.list_calls() >= 4);
}

#[test]
fn a_session_removed_keeps_the_view_and_its_reader_leaves_at_its_next_frame() {
    let (api, mut rig, streams) = known_rig(&["s1", "s2"]);
    let calls = api.list_calls();
    rig.handle.set_sessions(names(&["s1"])).unwrap();
    rig.sync();
    assert_eq!(rig.handle.snapshot().badge, Badge::Count(0), "no loss");
    assert_eq!(api.list_calls(), calls, "no fetch for a smaller set");
    // The retired reader is inside a read: the first frame it gets is not
    // passed on, and it ends.
    streams[1].send(requested("a9", "s2")).unwrap();
    let report = rig
        .driver
        .take()
        .expect("running")
        .shutdown(Duration::from_secs(2));
    assert_eq!(
        (report.panicked, report.detached),
        (0, 1),
        "only the reader of s1, still in its read"
    );
    assert_eq!(
        report.joined, 5,
        "the worker, three I/O threads, one reader"
    );
    drop(streams);
}

#[test]
fn a_session_added_with_every_slot_taken_waits_for_the_reader_that_leaves() {
    let (api, rig, streams) = known_rig(&["a", "b", "c", "d"]);
    let e = api.open_stream("e");
    rig.handle
        .set_sessions(names(&["a", "b", "c", "e"]))
        .unwrap();
    rig.until("e waits for a slot, covered by the refresh", |v| {
        v.parked_sessions == vec!["e".to_owned()]
    });
    assert_eq!(api.opens("e"), 0, "the retired reader holds its slot");
    assert_eq!(rig.handle.snapshot().badge, Badge::Count(0));
    // The worker has told the reader to leave once it has handled this.
    rig.sync();
    // What the retired reader sees before it leaves is not passed on.
    streams[3].send(requested("a9", "d")).unwrap();
    api.wait_opens("e", 1);
    rig.sync();
    assert_eq!(
        rig.handle.snapshot().waiting_at_least,
        0,
        "a9 was not applied"
    );
    Rig::up(&e);
    rig.tick_until("known with e followed", |v| {
        counted(v, 0) && v.parked_sessions.is_empty()
    });
    // No more streams were ever open than the cap.
    let total: usize = ["a", "b", "c", "d", "e"].iter().map(|s| api.opens(s)).sum();
    assert_eq!(total, MAX_STREAMS + 1, "four at first, one more for e");
}

#[test]
fn the_same_set_twice_changes_nothing() {
    let (api, rig, _s1) = known_rig(&["s1", "s2"]);
    let calls = api.list_calls();
    for same in [&["s1", "s2"][..], &["s2", "s1"], &["s1", "", "s2", "s1"]] {
        rig.handle.set_sessions(names(same)).unwrap();
        rig.sync();
    }
    assert_eq!(rig.handle.snapshot().badge, Badge::Count(0));
    assert_eq!((api.opens("s1"), api.opens("s2")), (1, 1));
    assert_eq!(api.list_calls(), calls);
}

#[test]
fn a_flood_of_set_sessions_is_never_refused_and_costs_no_more_streams_than_the_cap() {
    let (api, mut rig, s1) = known_rig(&["s1"]);
    let all: Vec<String> = (0..12).map(|i| format!("n{i}")).collect();
    // Four threads of the app call at once.
    std::thread::scope(|scope| {
        for t in 0..4usize {
            let (handle, all) = (rig.handle.clone(), &all);
            scope.spawn(move || {
                for i in 0..5_000usize {
                    let set = vec![all[(i + t) % 12].clone(), all[(i * 7 + 3) % 12].clone()];
                    assert!(handle.set_sessions(set).is_ok(), "never Busy");
                }
            });
        }
    });
    // The inbox is not clogged: other commands still get through.
    rig.sync();
    // Nobody ends the readers of the sessions that were dropped, so the
    // streams opened in all stay within the cap, however many changes came.
    let total: usize = (api.opens("s1")) + all.iter().map(|n| api.opens(n)).sum::<usize>();
    assert!(total <= MAX_STREAMS, "{total} streams were opened");
    // s1 was retired by the flood: it is followed again once its old reader
    // has left, by a reader of its own.
    let second = api.open_stream("s1");
    rig.handle.set_sessions(names(&["s1"])).unwrap();
    rig.sync();
    assert_eq!(api.opens("s1"), 1, "the old reader still holds its stream");
    s1[0].send(Frame::Item(StreamItem::Heartbeat)).unwrap();
    api.wait_opens("s1", 2);
    drop(second);
    let report = rig
        .driver
        .take()
        .expect("running")
        .shutdown(Duration::from_millis(300));
    assert_eq!(report.panicked, 0);
}

#[test]
fn the_newest_wish_replaces_one_the_worker_has_not_taken() {
    let store = Store::default();
    assert!(store.put_wanted(names(&["a"])), "none was waiting: wake it");
    assert!(!store.put_wanted(names(&["b"])), "one is waiting already");
    assert_eq!(store.take_wanted(), Some(names(&["b"])));
    assert_eq!(store.take_wanted(), None);
    assert!(store.put_wanted(names(&[])), "an empty set is a wish too");
    store.clear();
    assert_eq!(store.take_wanted(), None, "a stopped driver keeps none");
}

#[test]
fn ids_that_are_huge_or_have_control_characters_are_not_followed() {
    let (api, rig, _s1) = known_rig(&["s1"]);
    let huge = "x".repeat(1 << 20);
    let ok = api.open_stream("ok1");
    let hostile = vec![
        huge.clone(),
        "a\r\nHost: evil".to_owned(),
        "../etc/passwd".to_owned(),
        "ok1".to_owned(),
        "ok1".to_owned(),
    ];
    rig.handle.set_sessions(hostile).unwrap();
    api.wait_opens("ok1", 1);
    rig.sync();
    assert_eq!(api.opens(&huge), 0);
    assert_eq!(api.opens("a\r\nHost: evil"), 0);
    assert_eq!(api.opens("../etc/passwd"), 0);
    assert_eq!(api.opens("ok1"), 1, "twice in the list, one reader");
    Rig::up(&ok);
}

#[test]
fn a_parked_session_that_is_removed_ends_its_reader_at_once() {
    // Three refusals park the session; its reader then sleeps 30 s or more.
    let api = FakeApi::new();
    spare(&api, vec![]);
    for _ in 0..3 {
        let stream = api.open_stream("s1");
        stream
            .send(Frame::Fail(super::engine_tests::status(404)))
            .unwrap();
    }
    let mut rig = Rig::start(api.clone(), &["s1"]);
    rig.tick_until("parked", |v| v.parked_sessions == vec!["s1".to_owned()]);
    rig.handle.set_sessions(vec![]).unwrap();
    rig.until("no longer parked", |v| v.parked_sessions.is_empty());
    // The signal woke the sleeping reader: nothing is left to wait for.
    let report = rig.driver.take().expect("running").shutdown(PATIENCE);
    assert_eq!((report.panicked, report.detached), (0, 0));
    assert_eq!(report.joined, 5);
}

#[test]
fn shutdown_during_a_change_ends_cleanly_and_refuses_further_changes() {
    for round in 0..12 {
        let api = FakeApi::new();
        spare(&api, vec![]);
        let mut rig = Rig::start(api.clone(), &["s1"]);
        rig.handle.set_sessions(names(&["s2", "s3"])).unwrap();
        match round % 3 {
            0 => rig.sync(),
            1 => std::thread::yield_now(),
            _ => {}
        }
        let handle = rig.handle.clone();
        // The app keeps changing the set while the driver stops: every call
        // is answered, and refused once the driver is stopped.
        let hammer = {
            let handle = handle.clone();
            std::thread::spawn(move || {
                let mut i = 0usize;
                while handle.set_sessions(names(&[["a", "b"][i % 2]])).is_ok() {
                    i += 1;
                }
            })
        };
        let report = rig
            .driver
            .take()
            .expect("running")
            .shutdown(Duration::from_millis(300));
        hammer
            .join()
            .expect("the caller ends once the driver stopped");
        assert_eq!(report.panicked, 0, "round {round}");
        assert!(!handle.is_running());
        assert!(handle.set_sessions(names(&["s1"])).is_err(), "stopped");
        assert_eq!(handle.snapshot().badge, Badge::Unknown);
    }
}
