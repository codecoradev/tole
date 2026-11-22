//! The driver end to end: real threads, the real model, a scripted
//! [`FakeApi`] and a [`ManualClock`], so no socket and no real sleep. Waits are
//! on conditions (a call arrived, the view changed), bounded in real time only
//! so that a broken driver fails a test instead of hanging it.

use serde_json::json;
use tole_client::{Event, StreamItem};
use tole_desktop_model::approvals::{CardState, ComposerState, Decision, Outcome, Tab};
use tole_desktop_model::Badge;

use super::engine_tests::{io_err, list, reply, row, status};
use super::rig::{composers, counted, decide_rig, following, pending, spare, Rig};
use crate::driver::fake::{FakeApi, Frame};

#[test]
fn first_load_is_unknown_until_the_list_arrives() {
    let api = FakeApi::new();
    let gate = api.gate_list();
    let rig = Rig::start(api.clone(), &[]);
    api.wait_list_calls(1);
    let v = rig.handle.snapshot();
    assert_eq!(v.badge, Badge::Unknown);
    assert_eq!(rig.handle.composer("s1"), ComposerState::Checking);
    gate.send(Ok(list(pending()))).unwrap();
    let v = rig.until("the list is known", |v| counted(v, 1));
    assert_eq!(v.rows.len(), 1);
    assert_eq!(
        rig.handle.composer("s1"),
        ComposerState::WaitingForAnswer { requests: 1 }
    );
    assert_eq!(rig.handle.composer("s2"), ComposerState::Open);
    assert!(rig.handle.is_running());
}

#[test]
fn the_view_is_known_only_after_the_stream_is_up_and_a_fetch_followed() {
    let api = FakeApi::new();
    api.reply_list(Ok(list(pending())));
    api.reply_list(Ok(list(pending())));
    let stream = api.open_stream("s1");
    let rig = Rig::start(api.clone(), &["s1"]);
    api.wait_list_calls(1);
    rig.sync();
    assert_eq!(
        rig.handle.snapshot().badge,
        Badge::Unknown,
        "the first fetch began before the stream"
    );
    Rig::up(&stream);
    rig.tick_until_calls(&api, 2);
    rig.until("known", |v| counted(v, 1));
}

#[test]
fn a_stream_error_keeps_it_unknown_until_the_stream_is_back_and_a_fetch_followed() {
    let (api, rig, stream) = following(pending());
    let gate = api.gate_list();
    let second_stream = api.open_stream("s1");
    stream
        .send(Frame::Fail(io_err(std::io::ErrorKind::ConnectionReset)))
        .unwrap();
    rig.tick_until_calls(&api, 3);
    let v = rig.until("unknown after the loss", |v| v.badge == Badge::Unknown);
    assert_eq!(
        v.waiting_at_least, 1,
        "what was listed is kept, not trusted"
    );
    // The serve answers at once, but the stream is still down.
    gate.send(Ok(list(pending()))).unwrap();
    rig.sync();
    assert_eq!(
        rig.handle.snapshot().badge,
        Badge::Unknown,
        "no stream, no knowledge"
    );
    assert_eq!(rig.handle.composer("s2"), ComposerState::Checking);
    // The reader opens it again after its wait, and a fetch follows.
    spare(&api, pending());
    rig.tick_until_opens(&api, "s1", 2);
    Rig::up(&second_stream);
    rig.tick_until("known again", |v| counted(v, 1));
}

#[test]
fn a_reconnect_or_a_resync_or_a_closed_frame_is_followed_by_a_fetch() {
    let losses = [
        StreamItem::Reconnected { after: 7 },
        StreamItem::Event(Event::Resync { dropped: 2 }),
        StreamItem::Event(Event::Closed { id: "s1".into() }),
    ];
    for loss in losses {
        let (api, rig, stream) = following(pending());
        let gate = api.gate_list();
        stream.send(Frame::Item(loss)).unwrap();
        rig.tick_until_calls(&api, 3);
        rig.until("unknown until the fetch answers", |v| {
            v.badge == Badge::Unknown
        });
        gate.send(Ok(list(vec![]))).unwrap();
        rig.until("known again, the request gone", |v| counted(v, 0));
    }
}

#[test]
fn a_frame_on_the_stream_updates_the_view_without_a_fetch() {
    let (api, rig, stream) = following(vec![]);
    let calls = api.list_calls();
    let requested = row("a9", "s1", "pending");
    let event = Event::ApprovalRequested(serde_json::from_value(requested).unwrap());
    stream.send(Frame::Item(StreamItem::Event(event))).unwrap();
    rig.until("the request is counted", |v| counted(v, 1));
    assert_eq!(api.list_calls(), calls, "a live frame costs no call");
}

#[test]
fn a_failed_fetch_is_retried_with_a_growing_wait_then_known() {
    let api = FakeApi::new();
    for _ in 0..3 {
        api.reply_list(Err(io_err(std::io::ErrorKind::ConnectionRefused)));
    }
    api.reply_list(Ok(list(vec![])));
    let rig = Rig::start(api.clone(), &[]);
    api.wait_list_calls(1);
    // Never a call without the clock moving: it fails and waits.
    rig.sync();
    assert_eq!(api.list_calls(), 1);
    // Waits of 1 s, 2 s and 4 s: a retry needs at least that many ticks. (A
    // tick the worker handles late can only make it need more, so the bound
    // is a lower one; the exact steps are checked on the engine.)
    for (calls, secs) in (1usize..).zip([1usize, 2, 4]) {
        // Tick only once the failure of the last call has been handled.
        rig.until("the failure is counted", |v| {
            v.fetch_failures as usize == calls
        });
        let mut ticks = 0;
        while api.list_calls() == calls && ticks < secs + 2 {
            rig.tick();
            rig.sync(); // the worker has handled the tick before the next
            ticks += 1;
        }
        assert!(
            ticks >= secs,
            "retry after {ticks} ticks, the wait is {secs} s"
        );
        api.wait_list_calls(calls + 1);
    }
    let v = rig.until("known after the retries", |v| counted(v, 0));
    assert_eq!(v.fetch_failures, 0, "a success starts the count over");
}

#[test]
fn the_late_reply_of_a_superseded_fetch_does_not_make_the_list_known() {
    let api = FakeApi::new();
    let first = api.gate_list();
    let second = api.gate_list();
    let stream = api.open_stream("s1");
    let stream_again = api.open_stream("s1");
    let rig = Rig::start(api.clone(), &["s1"]);
    api.wait_list_calls(1);
    Rig::up(&stream);
    // The stream drops while fetch 1 is out.
    stream
        .send(Frame::Fail(io_err(std::io::ErrorKind::BrokenPipe)))
        .unwrap();
    first.send(Ok(list(pending()))).unwrap();
    let v = rig.until("the late reply merged", |v| v.waiting_at_least == 1);
    assert_eq!(
        v.badge,
        Badge::Unknown,
        "not known by a reply that began before the loss"
    );
    assert_eq!(rig.handle.composer("s2"), ComposerState::Checking);
    // The stream returns, the next fetch starts after it, and that one counts.
    rig.tick_until_opens(&api, "s1", 2);
    Rig::up(&stream_again);
    rig.tick_until_calls(&api, 2);
    // Whether fetch 2 began before or after the stream came back decides if it
    // counts; either way the list is known once a fetch follows the stream.
    spare(&api, pending());
    second.send(Ok(list(pending()))).unwrap();
    rig.tick_until("known by a fetch that follows the stream", |v| {
        counted(v, 1)
    });
}

#[test]
fn an_allow_is_sent_once_even_if_pressed_twice() {
    let (api, rig) = decide_rig();
    let gate = api.gate_decision();
    rig.handle.decide("a1", Decision::AllowOnce).unwrap();
    api.wait_decisions(1);
    rig.handle.decide("a1", Decision::AllowOnce).unwrap();
    rig.handle.decide("a1", Decision::DontAllow).unwrap();
    // The intermediate state: the card says it is being answered, with that
    // decision, and the box is shut.
    let v = rig.until("answering", |v| {
        v.selected
            .as_ref()
            .is_some_and(|c| c.state == CardState::Answering(Decision::AllowOnce))
    });
    assert_eq!(v.composers, composers("s1", 1));
    rig.sync();
    gate.send(Ok(reply("a1", "approved"))).unwrap();
    rig.until("decided", |v| counted(v, 0));
    rig.handle.select_tab(Tab::Decided).unwrap();
    let v = rig.until("the decided tab", |v| {
        v.tab == Tab::Decided && !v.rows.is_empty()
    });
    assert_eq!(v.rows[0].outcome, Some(Outcome::Allowed));
    assert_eq!(api.decisions().len(), 1, "one call, whatever the presses");
    assert_eq!(api.decisions()[0].1, tole_client::Decision::Allow);
}

#[test]
fn deny_and_a_conflict_and_a_network_failure() {
    // Deny.
    let (api, rig) = decide_rig();
    api.reply_decision(Ok(reply("a1", "denied")));
    rig.handle.decide("a1", Decision::DontAllow).unwrap();
    rig.until("denied", |v| counted(v, 0));
    assert_eq!(api.decisions()[0].1, tole_client::Decision::Deny);
    // A conflict: closed, then the refetch brings the server's word.
    let (api, rig) = decide_rig();
    api.reply_decision(Err(status(409)));
    api.reply_list(Ok(list(vec![row("a1", "s1", "denied")])));
    rig.handle.decide("a1", Decision::AllowOnce).unwrap();
    rig.tick_until_calls(&api, 2);
    rig.handle.select_tab(Tab::Decided).unwrap();
    let v = rig.until("the server's word", |v| {
        v.tab == Tab::Decided && v.rows.first().and_then(|r| r.outcome) == Some(Outcome::NotAllowed)
    });
    assert_eq!(v.badge, Badge::Count(0));
    // A network failure: it waits again, is looked at again, and may be pressed again.
    let (api, rig) = decide_rig();
    api.reply_decision(Err(io_err(std::io::ErrorKind::TimedOut)));
    api.reply_list(Ok(list(pending())));
    rig.handle.decide("a1", Decision::AllowOnce).unwrap();
    rig.tick_until_calls(&api, 2);
    rig.until("waiting again", |v| {
        counted(v, 1)
            && v.selected
                .as_ref()
                .is_some_and(|c| c.state == CardState::Waiting)
    });
    api.reply_decision(Ok(reply("a1", "approved")));
    rig.handle.decide("a1", Decision::AllowOnce).unwrap();
    rig.until("decided on the second try", |v| counted(v, 0));
    assert_eq!(api.decisions().len(), 2);
}

#[test]
fn a_helpers_decision_carries_its_fingerprint_to_the_call() {
    let api = FakeApi::new();
    let child = json!({"id": "dap-9", "source": "delegation", "status": "pending",
        "node": "n", "tool": "bash", "display": "run", "expiresInSeconds": 870,
        "fingerprint": "fp-abc"});
    api.reply_list(Ok(list(vec![child])));
    let rig = Rig::start(api.clone(), &[]);
    rig.until("known", |v| counted(v, 1));
    api.reply_decision(Ok(reply("dap-9", "allowed")));
    rig.handle.decide("dap-9", Decision::AllowOnce).unwrap();
    api.wait_decisions(1);
    assert_eq!(api.decisions()[0].2.as_deref(), Some("fp-abc"));
    rig.until("decided", |v| counted(v, 0));
}

#[test]
fn hostile_frames_and_replies_do_not_stop_the_driver() {
    let (api, rig, stream) = following(pending());
    api.reply_list(Err(status(500)));
    let bad = [
        json!(null),
        json!([1]),
        json!({"id": "../x", "status": "pending"}),
        json!({"id": 3}),
    ];
    for data in bad {
        let event = Event::Other {
            event: "approval_requested".into(),
            data,
        };
        stream.send(Frame::Item(StreamItem::Event(event))).unwrap();
    }
    stream.send(Frame::Item(StreamItem::Heartbeat)).unwrap();
    rig.until("unknown after the losses", |v| v.badge == Badge::Unknown);
    assert!(rig.handle.is_running());
    assert_ne!(rig.handle.composer("s9"), ComposerState::Open);
}

#[test]
fn the_stream_is_reopened_with_a_wait_after_it_ends() {
    let (api, rig, first) = following(vec![]);
    let again = api.open_stream("s1");
    drop(first); // a clean end
    rig.until("lost", |v| v.badge == Badge::Unknown);
    // The reader waits before it opens again: not before the clock moves.
    rig.sync();
    assert_eq!(api.opens("s1"), 1);
    rig.tick_until_opens(&api, "s1", 2);
    drop(again);
}
