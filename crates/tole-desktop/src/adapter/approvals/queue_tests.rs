//! The adapter driving the real queue: what the badge and the message box say
//! after each thing the stream and the fetches can do.

use serde_json::{json, Value};
use tole_client::{DecisionReply, Error, Event, StreamItem};
use tole_desktop_model::approvals::{
    ApprovalsAction, ApprovalsState, ComposerState, Decision, FetchId, Outcome, Scope, Tab,
};
use tole_desktop_model::Badge;

use super::queue_fixtures::*;
use super::*;

#[test]
fn the_list_is_known_only_after_the_snapshot_of_the_current_fetch() {
    let mut ids = FetchIds::new();
    let mut q = ApprovalsState::new();
    assert_eq!(q.waiting_badge(), Badge::Unknown);
    assert_eq!(q.composer("ses-1"), ComposerState::Checking);
    let (id, start) = ids.start(Scope::All).expect("id");
    q.apply(start);
    assert_eq!(q.waiting_badge(), Badge::Unknown, "started is not known");
    // The reply of a fetch that was never started is not trusted.
    q.apply(snapshot(&list(vec![]), Scope::All, FetchId(99), NOW));
    assert_eq!(q.waiting_badge(), Badge::Unknown);
    assert_eq!(q.composer("ses-1"), ComposerState::Checking);
    q.apply(snapshot(
        &list(vec![serve_row(), child_row()]),
        Scope::All,
        id,
        NOW,
    ));
    assert_eq!(q.waiting_badge(), Badge::Count(2));
    assert_eq!(
        q.composer("ses-1"),
        ComposerState::WaitingForAnswer { requests: 1 }
    );
    assert_eq!(q.composer("ses-2"), ComposerState::Open);
    assert_eq!(q.refused(), 0);
}

#[test]
fn a_failed_fetch_leaves_it_unknown_until_one_succeeds() {
    let mut ids = FetchIds::new();
    let mut q = known(&mut ids);
    let loss = apply_all(&mut q, &mut ids, vec![ApprovalsAction::ConnectionLost]);
    let first = loss.expect("a fetch follows a loss");
    q.apply(failed(Scope::All, first));
    assert_eq!(q.waiting_badge(), Badge::Unknown);
    assert_eq!(q.composer("ses-2"), ComposerState::Checking);
    // A late reply of the failed fetch changes nothing.
    q.apply(snapshot(&list(vec![]), Scope::All, first, NOW));
    assert_eq!(q.waiting_badge(), Badge::Unknown);
    let (second, start) = ids.start(Scope::All).expect("id");
    q.apply(start);
    q.apply(snapshot(&list(vec![]), Scope::All, second, NOW));
    assert_eq!(q.waiting_badge(), Badge::Count(0));
    assert_eq!(q.composer("ses-2"), ComposerState::Open);
}

#[test]
fn every_way_the_stream_is_lost_makes_the_queue_unknown_and_asks_for_a_fetch() {
    let losses: Vec<(&str, Vec<ApprovalsAction>)> = vec![
        (
            "reconnected",
            stream_actions(&StreamItem::Reconnected { after: 3 }, NOW),
        ),
        ("resync", event_actions(&Event::Resync { dropped: 2 }, NOW)),
        (
            "stream ended",
            vec![stream_error(&Error::ReconnectExhausted(5))],
        ),
        (
            "unreadable frame",
            event_actions(&undecoded("approval_resolved", json!({"status": 5})), NOW),
        ),
    ];
    for (name, actions) in losses {
        let mut ids = FetchIds::new();
        let mut q = known(&mut ids);
        assert_eq!(q.waiting_badge(), Badge::Count(2), "{name}");
        let id = apply_all(&mut q, &mut ids, actions).unwrap_or_else(|| panic!("{name}: no fetch"));
        assert_eq!(q.waiting_badge(), Badge::Unknown, "{name}");
        assert_eq!(q.composer("ses-2"), ComposerState::Checking, "{name}");
        assert!(
            !q.is_stale(),
            "{name}: the queue cannot ask, the driver must"
        );
        q.apply(snapshot(&list(vec![serve_row()]), Scope::All, id, NOW));
        assert_eq!(q.waiting_badge(), Badge::Count(1), "{name}");
        assert_eq!(q.composer("ses-2"), ComposerState::Open, "{name}");
    }
}

#[test]
fn a_loss_during_a_fetch_makes_its_reply_untrusted_and_a_new_fetch_is_needed() {
    let mut ids = FetchIds::new();
    let mut q = ApprovalsState::new();
    let (old, start) = ids.start(Scope::All).expect("id");
    q.apply(start);
    let new = apply_all(
        &mut q,
        &mut ids,
        event_actions(&Event::Resync { dropped: 1 }, NOW),
    )
    .expect("a new fetch");
    assert_ne!(old, new);
    q.apply(snapshot(&list(vec![]), Scope::All, old, NOW));
    assert_eq!(
        q.waiting_badge(),
        Badge::Unknown,
        "the old reply is not current"
    );
    q.apply(snapshot(&list(vec![]), Scope::All, new, NOW));
    assert_eq!(q.waiting_badge(), Badge::Count(0));
}

#[test]
fn only_a_loss_asks_for_a_fetch() {
    let mut ids = FetchIds::new();
    assert!(follow_up(&ApprovalsAction::Tick(1), &mut ids).is_none());
    assert!(follow_up(&ApprovalsAction::ConnectionLost, &mut ids).is_some());
    assert_eq!(ids.next_id(), Some(FetchId(2)), "one id was used");
}
#[test]
fn a_helper_request_allowed_here_reads_as_allowed() {
    let mut ids = FetchIds::new();
    let mut q = known(&mut ids);
    let intent = q.answer("dap-9", Decision::AllowOnce, NOW).expect("intent");
    assert_eq!(intent.fingerprint.as_deref(), Some("fp-abc"));
    // What serve's delegation route answers for an allow.
    let reply: DecisionReply =
        serde_json::from_value(json!({"id": "dap-9", "source": "delegation", "status": "allowed"}))
            .expect("reply");
    q.apply(ApprovalsAction::Resolved(
        resolved_from_reply(&reply),
        NOW + 1,
    ));
    let decided = q.rows(Tab::Decided, NOW + 1);
    assert_eq!(decided[0].id, "dap-9");
    assert_eq!(decided[0].outcome, Some(Outcome::Allowed));
    assert_eq!(
        q.waiting_badge(),
        Badge::Count(1),
        "known, one request left"
    );
    assert_eq!(status_of("dap-9", "allowed"), "approved");
    assert_eq!(status_of("dap-9", "denied"), "denied");
    // Serve's own queue never says `allowed`: not translated there.
    assert_eq!(status_of("apr-1", "allowed"), "allowed");
}

#[test]
fn a_helper_request_denied_here_reads_as_not_allowed() {
    let mut ids = FetchIds::new();
    let mut q = known(&mut ids);
    q.answer("dap-9", Decision::DontAllow, NOW).expect("intent");
    let reply: DecisionReply =
        serde_json::from_value(json!({"id": "dap-9", "source": "delegation", "status": "denied"}))
            .expect("reply");
    q.apply(ApprovalsAction::Resolved(
        resolved_from_reply(&reply),
        NOW + 1,
    ));
    assert_eq!(
        q.rows(Tab::Decided, NOW + 1)[0].outcome,
        Some(Outcome::NotAllowed)
    );
}

#[test]
fn a_serve_reply_and_frames_keep_their_words() {
    let mut ids = FetchIds::new();
    let mut q = known(&mut ids);
    q.answer("apr-1", Decision::AllowOnce, NOW).expect("intent");
    let reply: DecisionReply =
        serde_json::from_value(json!({"id": "apr-1", "status": "approved", "resuming": true}))
            .expect("reply");
    q.apply(ApprovalsAction::Resolved(
        resolved_from_reply(&reply),
        NOW + 1,
    ));
    assert_eq!(
        q.rows(Tab::Decided, NOW + 1)[0].outcome,
        Some(Outcome::Allowed)
    );
}

#[test]
fn frames_that_are_not_about_approvals_make_no_action() {
    assert!(stream_actions(&StreamItem::Heartbeat, NOW).is_empty());
    let other = Event::Other {
        event: "approval_future".into(),
        data: Value::Null,
    };
    assert!(event_actions(&other, NOW).is_empty());
    assert!(event_actions(&Event::TextDelta { text: "x".into() }, NOW).is_empty());
    let requested = serde_json::from_value(serve_row()).expect("row");
    match event_actions(&Event::ApprovalRequested(requested), 5).as_slice() {
        [ApprovalsAction::Requested(r, 5)] => assert_eq!(r.id, "apr-1"),
        other => panic!("{other:?}"),
    }
}

#[test]
fn an_unknown_status_frame_is_never_pending_or_allowed() {
    let mut q = ApprovalsState::new();
    let row = serde_json::from_value(serve_row()).expect("row");
    q.apply(ApprovalsAction::Requested(request(&row), 1));
    assert_eq!(q.waiting_count(), 1);
    let frame = Event::ApprovalResolved(
        serde_json::from_value(
            json!({"id": "apr-1", "tool": "write_file", "status": "teleported"}),
        )
        .expect("row"),
    );
    for a in event_actions(&frame, 2) {
        q.apply(a);
    }
    assert_eq!(q.waiting_count(), 0, "not pending");
    let rows = q.rows(Tab::Decided, 2);
    assert_eq!(
        rows[0].outcome,
        Some(Outcome::Closed),
        "never worded as allowed"
    );
}

#[test]
fn the_queue_asks_for_a_fetch_by_itself_when_one_is_given_up_or_the_list_is_stale() {
    let mut ids = FetchIds::new();
    let mut q = ApprovalsState::new();
    assert!(due(&q, &mut ids).is_none(), "nothing is asked at the start");
    let (_, start) = ids.start(Scope::All).expect("id");
    q.apply(start);
    // The reply never comes and nothing says it failed: the clock gives up.
    q.apply(ApprovalsAction::Tick(NOW));
    assert!(due(&q, &mut ids).is_none(), "a fetch runs");
    let wait = tole_desktop_model::approvals::FETCH_TIMEOUT_SECS * 1000;
    q.apply(ApprovalsAction::Tick(NOW + wait));
    let (id, start) = due(&q, &mut ids).expect("a new fetch is asked for");
    q.apply(start);
    assert!(due(&q, &mut ids).is_none(), "and it runs now");
    let rows = list(vec![serve_row()]);
    q.apply(snapshot(&rows, Scope::All, id, NOW + wait + 1));
    assert_eq!(q.waiting_badge(), Badge::Count(1));
    assert!(due(&q, &mut ids).is_none());
    // A conflict makes the list stale.
    q.answer("apr-1", Decision::AllowOnce, NOW + wait + 3)
        .expect("answerable");
    let failed = ApprovalsAction::AnswerFailed("apr-1".into(), true, NOW + wait + 4);
    q.apply(failed);
    assert!(due(&q, &mut ids).is_some());
}
