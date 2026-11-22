//! The owner's answers through the engine and the real model: what is sent,
//! once, and what each kind of reply does to the card, the badge and the
//! message box. Also the engine against data a hostile server could send.

use std::io;

use serde_json::json;
use tole_client::{Error, Event, StreamItem};
use tole_desktop_model::approvals::{ComposerState, Decision, DecisionIntent, Outcome, Tab};
use tole_desktop_model::Badge;

use super::engine::{Input, Now};
use super::engine_tests::{fetch_of, io_err, list, reply, row, sent, status, T};

fn press(t: &mut T, id: &str, decision: Decision) -> Vec<DecisionIntent> {
    let out = t.step(Input::Decide(id.into(), decision));
    sent(&out).into_iter().cloned().collect()
}

fn child_row() -> serde_json::Value {
    json!({
        "id": "dap-9", "source": "delegation", "status": "pending", "node": "helper-1",
        "tool": "bash", "display": "run: rm x", "expiresInSeconds": 870,
        "fingerprint": "fp-abc"
    })
}

#[test]
fn allow_is_sent_once_and_the_servers_word_is_applied() {
    let mut t = T::known(vec![row("a1", "s1", "pending")]);
    let intents = press(&mut t, "a1", Decision::AllowOnce);
    assert_eq!(intents.len(), 1);
    assert_eq!(
        (intents[0].id.as_str(), intents[0].decision),
        ("a1", Decision::AllowOnce)
    );
    // While the answer is on its way the box stays shut and nothing is decided.
    assert_eq!(
        t.store.lock().composer("s1"),
        ComposerState::WaitingForAnswer { requests: 1 }
    );
    assert_eq!(t.decided("a1"), None);
    let out = t.step(Input::Decided(
        intents[0].clone(),
        Ok(reply("a1", "approved")),
    ));
    assert!(out.effects.is_empty(), "a decision needs no refetch");
    assert_eq!(t.decided("a1"), Some(Outcome::Allowed));
    assert_eq!(t.view().badge, Badge::Count(0));
    assert_eq!(t.store.lock().composer("s1"), ComposerState::Open);
}

#[test]
fn deny_is_a_deny() {
    let mut t = T::known(vec![row("a1", "s1", "pending")]);
    let intents = press(&mut t, "a1", Decision::DontAllow);
    assert_eq!(intents[0].decision, Decision::DontAllow);
    t.step(Input::Decided(
        intents[0].clone(),
        Ok(reply("a1", "denied")),
    ));
    assert_eq!(t.decided("a1"), Some(Outcome::NotAllowed));
}

#[test]
fn the_server_word_beats_the_button_when_somebody_answered_first() {
    // Allow sent, the server says another client denied it first.
    let mut t = T::known(vec![row("a1", "s1", "pending")]);
    let intents = press(&mut t, "a1", Decision::AllowOnce);
    t.step(Input::Decided(
        intents[0].clone(),
        Ok(reply("a1", "denied")),
    ));
    assert_eq!(t.decided("a1"), Some(Outcome::NotAllowed));
}

#[test]
fn a_double_press_sends_one_decision() {
    let mut t = T::known(vec![row("a1", "s1", "pending")]);
    assert_eq!(press(&mut t, "a1", Decision::AllowOnce).len(), 1);
    // Again, and the opposite, while the first is out and after it was sent.
    assert!(press(&mut t, "a1", Decision::AllowOnce).is_empty());
    assert!(press(&mut t, "a1", Decision::DontAllow).is_empty());
    // And after it was answered.
    t.step(Input::Decided(
        DecisionIntent {
            id: "a1".into(),
            decision: Decision::AllowOnce,
            fingerprint: None,
        },
        Ok(reply("a1", "approved")),
    ));
    assert!(press(&mut t, "a1", Decision::DontAllow).is_empty());
    assert_eq!(
        t.decided("a1"),
        Some(Outcome::Allowed),
        "the first answer wins"
    );
    // A request nobody knows is nothing to answer.
    assert!(press(&mut t, "nope", Decision::AllowOnce).is_empty());
}

#[test]
fn a_conflict_leaves_it_closed_and_the_refetch_brings_the_servers_word() {
    let mut t = T::known(vec![row("a1", "s1", "pending")]);
    let intents = press(&mut t, "a1", Decision::AllowOnce);
    t.step(Input::Decided(intents[0].clone(), Err(status(409))));
    assert_eq!(
        t.decided("a1"),
        Some(Outcome::Closed),
        "no longer waiting, not allowed"
    );
    assert!(!t.waiting("a1"));
    let id = t
        .fetch_within(1_500)
        .expect("a conflict makes the list stale: refetch");
    t.step(Input::Fetched(
        id,
        Ok(list(vec![row("a1", "s1", "denied")])),
    ));
    assert_eq!(t.decided("a1"), Some(Outcome::NotAllowed));
}

#[test]
fn a_network_failure_brings_the_buttons_back_and_looks_again() {
    for e in [
        io_err(io::ErrorKind::TimedOut),
        status(500),
        io_err(io::ErrorKind::BrokenPipe),
    ] {
        let mut t = T::known(vec![row("a1", "s1", "pending")]);
        let intents = press(&mut t, "a1", Decision::AllowOnce);
        t.step(Input::Decided(intents[0].clone(), Err(e)));
        assert!(t.waiting("a1"), "it waits again");
        assert_eq!(t.decided("a1"), None);
        assert!(
            t.fetch_within(1_500).is_some(),
            "the call may have landed: look again"
        );
        // The owner may press again, and it is sent again (once).
        assert_eq!(press(&mut t, "a1", Decision::AllowOnce).len(), 1);
    }
}

#[test]
fn a_reply_that_is_not_about_this_request_adds_nothing() {
    for bad in [reply("apr-evil", "approved"), reply("a1", "pending")] {
        let mut t = T::known(vec![row("a1", "s1", "pending")]);
        let intents = press(&mut t, "a1", Decision::AllowOnce);
        t.step(Input::Decided(intents[0].clone(), Ok(bad)));
        assert!(
            t.waiting("a1"),
            "not decided by a reply that is not its decision"
        );
        assert_eq!(t.decided("apr-evil"), None);
        assert_eq!(t.store.lock().rows(Tab::Decided, t.now.wall).len(), 0);
    }
}

#[test]
fn a_status_this_version_does_not_know_is_closed_never_allowed() {
    let mut t = T::known(vec![row("a1", "s1", "pending")]);
    let intents = press(&mut t, "a1", Decision::AllowOnce);
    t.step(Input::Decided(intents[0].clone(), Ok(reply("a1", "maybe"))));
    assert_eq!(t.decided("a1"), Some(Outcome::Closed));
}

#[test]
fn a_helpers_request_carries_its_fingerprint_and_its_allowed_reads_as_approved() {
    let mut t = T::known(vec![child_row()]);
    let intents = press(&mut t, "dap-9", Decision::AllowOnce);
    assert_eq!(intents[0].fingerprint.as_deref(), Some("fp-abc"));
    t.step(Input::Decided(
        intents[0].clone(),
        Ok(reply("dap-9", "allowed")),
    ));
    assert_eq!(t.decided("dap-9"), Some(Outcome::Allowed));
}

#[test]
fn a_request_whose_time_is_up_is_not_sent() {
    let mut t = T::known(vec![row("a1", "s1", "pending")]);
    t.after(901_000);
    assert!(press(&mut t, "a1", Decision::AllowOnce).is_empty());
    assert_eq!(t.decided("a1"), Some(Outcome::Expired));
}

#[test]
fn decisions_go_out_one_at_a_time_in_order() {
    let mut t = T::known(vec![row("a1", "s1", "pending"), row("a2", "s2", "pending")]);
    let first = press(&mut t, "a1", Decision::AllowOnce);
    assert_eq!(first.len(), 1);
    assert!(
        press(&mut t, "a2", Decision::DontAllow).is_empty(),
        "queued behind the first"
    );
    let out = t.step(Input::Decided(
        first[0].clone(),
        Ok(reply("a1", "approved")),
    ));
    let next = sent(&out);
    assert_eq!(next.len(), 1);
    assert_eq!(
        (next[0].id.as_str(), next[0].decision),
        ("a2", Decision::DontAllow)
    );
}

#[test]
fn hostile_server_data_never_panics_and_never_opens_a_box() {
    let mut t = T::known(vec![row("a1", "s1", "pending")]);
    let huge = "x".repeat(100_000);
    let frames = vec![
        json!(null),
        json!([1, 2, 3]),
        json!("text"),
        json!({"id": 5}),
        json!({"id": "../../etc/passwd", "status": "pending", "tool": "bash"}),
        json!({"id": huge, "status": "pending"}),
        json!({"id": "ok-1", "status": "pending", "tool": null, "expiresInSeconds": -1,
               "createdAtMs": 1e300, "sessionId": {"a": 1}}),
        json!({"id": "ok-2", "status": "pending", "createdAtMs": u64::MAX,
               "expiresInSeconds": u64::MAX}),
    ];
    for data in frames {
        for name in ["approval_requested", "approval_resolved"] {
            let event = Event::Other {
                event: name.into(),
                data: data.clone(),
            };
            t.step(Input::Stream(StreamItem::Event(event)));
        }
    }
    // Whatever came, the list is not claimed to be known until a fetch says so,
    // and a request that may be pending keeps the badge from being a number.
    t.step(Input::Stream(StreamItem::Reconnected { after: 0 }));
    assert_ne!(t.view().badge, Badge::Count(0));
    // Time at its extremes.
    t.now = Now {
        wall: u64::MAX - 1,
        mono: u64::MAX - 1,
    };
    t.step(Input::Tick);
    t.step(Input::Tick);
    // Every kind of error is a failed fetch or a failed answer, nothing else.
    for e in [
        io_err(io::ErrorKind::OutOfMemory),
        Error::Protocol("x".into()),
        Error::Decode(serde_json::from_str::<i32>("x").unwrap_err()),
        Error::ReconnectExhausted(u32::MAX),
        Error::InvalidConfig("x".into()),
    ] {
        t.step(Input::Decided(
            DecisionIntent {
                id: "a1".into(),
                decision: Decision::AllowOnce,
                fingerprint: None,
            },
            Err(e),
        ));
    }
    assert!(t
        .view()
        .composers
        .iter()
        .all(|(_, c)| *c != ComposerState::Open));
}

#[test]
fn a_flood_of_requests_is_bounded_and_the_badge_says_unknown() {
    let rows: Vec<_> = (0..5_000)
        .map(|i| row(&format!("a{i}"), "s1", "pending"))
        .collect();
    let mut t = T::new();
    let id = fetch_of(&t.start()).expect("fetch");
    t.step(Input::Fetched(id, Ok(list(rows))));
    let v = t.view();
    assert!(v.rows.len() <= tole_desktop_model::approvals::MAX_WAITING);
    assert_eq!(
        v.badge,
        Badge::Unknown,
        "requests over the cap are uncounted"
    );
}

fn two_requests() -> T {
    T::known(vec![row("a1", "s1", "pending"), row("a2", "s2", "pending")])
}

#[test]
fn a_queued_intent_is_dropped_when_its_request_expired_before_it_left() {
    let mut t = two_requests();
    let first = press(&mut t, "a1", Decision::AllowOnce);
    assert_eq!(first.len(), 1);
    // a2 waits behind the call of a1, and its time runs out meanwhile.
    assert!(press(&mut t, "a2", Decision::AllowOnce).is_empty());
    t.after(901_000);
    assert_eq!(t.decided("a2"), Some(Outcome::Expired));
    // The call of a1 ends; the allow of a2 must not go out for a card that
    // reads Expired.
    let out = t.step(Input::Decided(first[0].clone(), Err(status(500))));
    assert!(sent(&out).is_empty(), "an expired request is not allowed");
}

#[test]
fn a_queued_intent_is_dropped_when_somebody_else_answered_first() {
    let mut t = two_requests();
    let first = press(&mut t, "a1", Decision::AllowOnce);
    assert!(press(&mut t, "a2", Decision::AllowOnce).is_empty());
    let frame = Event::ApprovalResolved(serde_json::from_value(row("a2", "s2", "denied")).unwrap());
    t.step(super::engine_tests::stream(frame));
    let out = t.step(Input::Decided(
        first[0].clone(),
        Ok(reply("a1", "approved")),
    ));
    assert!(sent(&out).is_empty(), "it could only answer 409");
    assert_eq!(t.decided("a2"), Some(Outcome::NotAllowed));
}

#[test]
fn the_owners_last_press_is_the_one_that_is_sent() {
    let mut t = two_requests();
    let first = press(&mut t, "a1", Decision::AllowOnce);
    assert!(press(&mut t, "a2", Decision::AllowOnce).is_empty());
    // a2 expires by the local clock while it is queued; the server still says
    // it is pending, so a fetch revives it and the owner presses Don't allow.
    // The list is older than twice the refresh by now: it is fetched again.
    let id = fetch_of(&t.after(901_000)).expect("the refresh");
    let revived = json!({
        "id": "a2", "sessionId": "s2", "tool": "write_file", "status": "pending",
        "createdAtMs": t.now.wall, "expiresInSeconds": 900
    });
    t.step(Input::Fetched(
        id,
        Ok(list(vec![row("a1", "s1", "pending"), revived])),
    ));
    assert!(t.waiting("a2"), "revived by the server's word");
    assert!(
        press(&mut t, "a2", Decision::DontAllow).is_empty(),
        "queued behind a1"
    );
    let out = t.step(Input::Decided(first[0].clone(), Err(status(500))));
    let next = sent(&out);
    assert_eq!(next.len(), 1, "one send, not two");
    assert_eq!(
        (next[0].id.as_str(), next[0].decision),
        ("a2", Decision::DontAllow)
    );
}

#[test]
fn the_same_press_twice_while_queued_is_one_send() {
    let mut t = two_requests();
    let first = press(&mut t, "a1", Decision::AllowOnce);
    assert!(press(&mut t, "a2", Decision::DontAllow).is_empty());
    assert!(press(&mut t, "a2", Decision::DontAllow).is_empty());
    let out = t.step(Input::Decided(
        first[0].clone(),
        Ok(reply("a1", "approved")),
    ));
    assert_eq!(sent(&out).len(), 1);
}

#[test]
fn a_queued_allow_does_not_leave_after_its_time_even_between_two_ticks() {
    let mut t = two_requests();
    let first = press(&mut t, "a1", Decision::AllowOnce);
    assert!(press(&mut t, "a2", Decision::AllowOnce).is_empty());
    // The model's last tick was 300 ms before a2's deadline (15 minutes after
    // it was created, 500 ms before T0); the call of a1 ends 300 ms after it,
    // 600 ms after that tick, so no tick is due: the dispatch must look at the
    // time itself.
    let deadline = super::engine_tests::T0 - 500 + 900_000;
    t.now.wall = deadline - 300;
    t.now.mono += 899_000;
    t.step(Input::Tick);
    t.now.wall = deadline + 300;
    t.now.mono += 600;
    let out = t.step(Input::Decided(first[0].clone(), Err(status(500))));
    assert!(sent(&out).is_empty(), "300 ms past the deadline");
    assert_eq!(t.decided("a2"), Some(Outcome::Expired));
}

#[test]
fn a_failed_decision_asks_for_a_fetch_soon_even_when_a_refresh_is_already_out() {
    let mut t = T::known(vec![row("a1", "s1", "pending")]);
    // The refresh starts; then the owner presses and the call fails in a way
    // that may have landed; the refresh that began before it comes back.
    let refresh = fetch_of(&t.after(15_000)).expect("the refresh");
    let intents = press(&mut t, "a1", Decision::AllowOnce);
    t.step(Input::Decided(
        intents[0].clone(),
        Err(io_err(io::ErrorKind::TimedOut)),
    ));
    t.step(Input::Fetched(
        refresh,
        Ok(list(vec![row("a1", "s1", "pending")])),
    ));
    // Not 15 s later: the next fetch comes within the pace.
    assert!(t.fetch_within(1_500).is_some(), "look again soon");
}
