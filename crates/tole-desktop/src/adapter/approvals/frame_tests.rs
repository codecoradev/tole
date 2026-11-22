//! Frames the client could not read, the end of the stream and the retry of a
//! failed fetch, through the real queue.

use serde_json::{json, Value};
use tole_client::Event;
use tole_desktop_model::approvals::{
    strings, ApprovalsAction, ApprovalsState, CardState, Care, ComposerState, Outcome, Scope, Tab,
    Wording,
};
use tole_desktop_model::Badge;

use super::queue_fixtures::*;
use super::*;

/// Undecodable `approval_requested` data: each makes the client's frame decoder
/// return `Event::Other`, and the request must still reach the owner.
fn bad_requested() -> Vec<(&'static str, Value)> {
    vec![
        (
            "summary without effect",
            json!({"id": "apr-7", "sessionId": "ses-1", "tool": "bash", "status": "pending",
                   "summary": {"kind": "run_command", "target": "rm -rf x"}}),
        ),
        (
            "fractional expiry",
            json!({"id": "apr-7", "sessionId": "ses-1", "tool": "bash", "status": "pending",
                   "expiresInSeconds": 900.5}),
        ),
        (
            "tool null",
            json!({"id": "apr-7", "sessionId": "ses-1", "tool": null, "status": "pending"}),
        ),
        (
            "negative time",
            json!({"id": "apr-7", "sessionId": "ses-1", "tool": "bash", "status": "pending",
                   "createdAtMs": -5}),
        ),
    ]
}

#[test]
fn an_undecodable_approval_requested_is_shown_and_makes_the_list_unknown() {
    for (name, data) in bad_requested() {
        let event = undecoded("approval_requested", data);
        assert!(
            matches!(event, Event::Other { .. }),
            "{name}: the client could not decode it"
        );
        let mut ids = FetchIds::new();
        let mut q = known(&mut ids);
        assert_eq!(
            q.composer("ses-1"),
            ComposerState::WaitingForAnswer { requests: 1 }
        );
        let fetch = apply_all(&mut q, &mut ids, event_actions(&event, NOW));
        assert!(fetch.is_some(), "{name}: a fetch follows");
        assert_eq!(q.waiting_count(), 3, "{name}: the request is listed");
        assert_eq!(q.waiting_badge(), Badge::Unknown, "{name}");
        let rows = q.rows(Tab::Waiting, NOW);
        let row = rows.iter().find(|r| r.id == "apr-7").expect("a row");
        assert_eq!(row.wording, Wording::Unknown, "{name}");
        assert_eq!(row.care, Care::Danger, "{name}");
        // The action is not known, so the card says so rather than guessing.
        let card = q.card("apr-7", NOW).expect("a card");
        assert_eq!(card.wording, Wording::Unknown, "{name}");
        assert_eq!(card.care, Care::Danger, "{name}");
        assert_eq!(card.state, CardState::Waiting, "{name}");
        assert!(
            !card.subject.is_empty(),
            "{name}: something names the request"
        );
        assert_eq!(
            q.composer("ses-1"),
            ComposerState::WaitingForAnswer { requests: 2 },
            "{name}"
        );
    }
}

#[test]
fn an_unreadable_tool_is_the_unknown_tool_with_the_frame_as_its_text() {
    for tool in [json!(null), json!({"name": "bash"}), json!(["bash"])] {
        let event = undecoded(
            "approval_requested",
            json!({"id": "apr-7", "sessionId": "ses-1", "tool": tool, "status": "pending",
                   "description": ["rm", "-rf"], "expiresInSeconds": 900.5}),
        );
        let actions = event_actions(&event, NOW);
        let ApprovalsAction::Requested(r, _) = &actions[0] else {
            panic!("{actions:?}");
        };
        assert!(!r.tool.is_empty());
        // A wrong-typed description is its JSON text, not "not reported".
        assert_eq!(r.description.as_deref(), Some("[\"rm\",\"-rf\"]"));
    }
    let event = undecoded(
        "approval_requested",
        json!({"id": "apr-7", "tool": null, "status": "pending", "x": 1}),
    );
    let ApprovalsAction::Requested(r, _) = &event_actions(&event, NOW)[0] else {
        panic!("a requested row");
    };
    assert_eq!(r.tool, "unknown");
    let text = r.description.as_deref().unwrap_or("");
    assert!(text.contains("apr-7") && text.contains("\"x\":1"), "{text}");
}

#[test]
fn a_rebuilt_request_keeps_its_times_so_the_countdown_is_the_servers() {
    // Created 600 s ago with the whole 900 s wait: about 300 s are left.
    let created = NOW - 600_000;
    for expires in [json!(900), json!(900.5)] {
        let event = undecoded(
            "approval_requested",
            json!({"id": "apr-7", "sessionId": "ses-1", "tool": "bash", "status": "pending",
                   "createdAtMs": created, "expiresInSeconds": expires,
                   "summary": {"kind": "run_command"}}),
        );
        assert!(matches!(event, Event::Other { .. }));
        let mut ids = FetchIds::new();
        let mut q = known(&mut ids);
        apply_all(&mut q, &mut ids, event_actions(&event, NOW));
        let card = q.card("apr-7", NOW).expect("a card");
        assert_eq!(card.left_short, Some(strings::left_short(300)), "{expires}");
        assert_ne!(card.left_short, Some(strings::left_short(900)));
    }
    // Negative and non-number times are absent, never guessed.
    let event = undecoded(
        "approval_requested",
        json!({"id": "apr-7", "tool": "bash", "createdAtMs": -5, "expiresInSeconds": "9"}),
    );
    let ApprovalsAction::Requested(r, _) = &event_actions(&event, NOW)[0] else {
        panic!("a requested row");
    };
    assert_eq!((r.created_at_ms, r.expires_in_secs), (None, None));
}

#[test]
fn a_closed_session_and_a_clean_end_are_losses() {
    let closed = event_actions(&Event::Closed { id: "ses-1".into() }, NOW);
    assert_eq!(closed, vec![ApprovalsAction::ConnectionLost]);
    for actions in [closed, vec![stream_ended()]] {
        let mut ids = FetchIds::new();
        let mut q = known(&mut ids);
        assert_eq!(q.waiting_badge(), Badge::Count(2));
        let id = apply_all(&mut q, &mut ids, actions).expect("a fetch follows");
        assert_eq!(q.waiting_badge(), Badge::Unknown);
        assert_eq!(q.composer("ses-2"), ComposerState::Checking);
        q.apply(snapshot(&list(vec![serve_row()]), Scope::All, id, NOW));
        assert_eq!(q.waiting_badge(), Badge::Count(1));
    }
}

#[test]
fn a_failed_first_fetch_is_retried_with_a_bounded_backoff_until_it_succeeds() {
    let mut ids = FetchIds::new();
    let mut backoff = Backoff::new();
    let mut q = ApprovalsState::new();
    let mut waits = Vec::new();
    for _ in 0..8 {
        let (id, start) = ids.start(Scope::All).expect("id");
        q.apply(start);
        q.apply(failed(Scope::All, id));
        // The queue never asks for a retry, and follow_up cannot: the driver does.
        assert!(!q.is_stale());
        assert!(follow_up(&failed(Scope::All, id), &mut ids).is_none());
        assert_eq!(q.waiting_badge(), Badge::Unknown);
        assert_eq!(q.composer("ses-1"), ComposerState::Checking);
        waits.push(backoff.failed().as_secs());
    }
    assert_eq!(waits, [1, 2, 4, 8, 16, 30, 30, 30]);
    assert_eq!(backoff.failures(), 8);
    let (id, start) = ids.start(Scope::All).expect("id");
    q.apply(start);
    q.apply(snapshot(&list(vec![]), Scope::All, id, NOW));
    backoff.succeeded();
    assert_eq!(q.composer("ses-1"), ComposerState::Open);
    assert_eq!(
        backoff.failed(),
        RETRY_MIN,
        "a new sequence starts at the minimum"
    );
}

#[test]
fn the_backoff_never_exceeds_its_bound_or_overflows() {
    let mut backoff = Backoff::new();
    for _ in 0..200 {
        let d = backoff.failed();
        assert!(d >= RETRY_MIN && d <= RETRY_MAX);
    }
    let mut spent = Backoff::after(u32::MAX);
    assert_eq!(spent.failed(), RETRY_MAX);
}

#[test]
fn an_undecodable_approval_requested_without_an_id_is_only_a_loss() {
    let event = undecoded(
        "approval_requested",
        json!({"tool": 4, "status": "pending"}),
    );
    let actions = event_actions(&event, NOW);
    assert_eq!(actions, vec![ApprovalsAction::ConnectionLost]);
}

#[test]
fn an_undecodable_approval_resolved_is_applied_when_it_says_enough() {
    let mut ids = FetchIds::new();
    let mut q = known(&mut ids);
    let event = undecoded(
        "approval_resolved",
        json!({"id": "apr-1", "sessionId": "ses-1", "tool": "write_file",
               "status": "denied", "expiresInSeconds": 900.5}),
    );
    assert!(matches!(event, Event::Other { .. }));
    let actions = event_actions(&event, NOW);
    assert!(matches!(
        actions.as_slice(),
        [ApprovalsAction::Resolved(..)]
    ));
    assert!(
        apply_all(&mut q, &mut ids, actions).is_none(),
        "no loss: nothing is missing"
    );
    assert_eq!(q.waiting_badge(), Badge::Count(1));
    let decided = q.rows(Tab::Decided, NOW);
    assert_eq!(decided[0].outcome, Some(Outcome::NotAllowed));
}
