//! Second review: a bare `approval_required` reply, post-send failures, the pace
//! of the retries, the bound on live approval frames, and two turn-error shapes.

use super::*;
use crate::adapter::task::{
    approvals_backoff, prompt_outcome, BusyPolling, PromptOutcome, ResumePolling,
    APPROVALS_RETRY_BASE, APPROVALS_RETRY_MAX, MAX_APPROVALS_RETRIES, MAX_REFRESH_ROWS,
};
use std::time::Duration;
use tole_client::{Error, PromptReply, SessionStatus};
use tole_desktop_model::task::{Enter, EnterOutcome, Phase};

fn status(busy: bool) -> Result<SessionStatus, Error> {
    let row = json!({"id": "s1", "entries": null, "busy": busy, "usage_report": {},
        "model": null, "effort": null, "contextWindow": null, "contextUsed": null,
        "name": null, "snapshotAt": null, "stale": false});
    Ok(serde_json::from_value(row).expect("a status"))
}

fn phase(t: &Task) -> Phase {
    t.state.status().phase
}

fn send(t: &mut Task) {
    t.state.composer.text = "go".into();
    let enter = Enter {
        shift: false,
        composing: false,
    };
    assert!(matches!(
        t.state.press_enter(enter),
        EnterOutcome::Submit(_)
    ));
}

/// The body serve sends when the request was decided before the reply was built.
fn bare_reply() -> Result<PromptReply, Error> {
    Ok(serde_json::from_value(json!({"stopReason": "approval_required"})).unwrap())
}

#[test]
fn a_bare_approval_required_reply_does_not_end_a_turn_the_allow_resumed() {
    let mut t = Task::new();
    send(&mut t);
    t.live(&[
        approval_frame("approval_requested", "a1", "pending"),
        approval_frame("approval_resolved", "a1", "approved"),
    ]);
    assert_eq!(phase(&t), Phase::Resuming);
    let PromptOutcome::Ended {
        events,
        check_status,
    } = prompt_outcome(&bare_reply())
    else {
        panic!()
    };
    assert!(events.is_empty() && check_status);
    t.apply(events);
    assert_eq!(phase(&t), Phase::Resuming, "the running write is not Idle");
    // The allow's own polling confirms it, and the end settles it.
    let mut poll = ResumePolling::new();
    poll.status(&status(true), Duration::from_secs(1))
        .events
        .into_iter()
        .for_each(|e| t.state.apply(e));
    assert_eq!(phase(&t), Phase::Working);
}

#[test]
fn a_bare_reply_after_a_denial_stays_idle_and_one_with_a_lost_frame_is_settled_by_the_status() {
    let mut denied = Task::new();
    send(&mut denied);
    denied.live(&[
        approval_frame("approval_requested", "a1", "pending"),
        approval_frame("approval_resolved", "a1", "denied"),
    ]);
    let PromptOutcome::Ended { events, .. } = prompt_outcome(&bare_reply()) else {
        panic!()
    };
    denied.apply(events);
    assert_eq!(phase(&denied), Phase::Idle);
    // The decision frame never came: the turn is still Working, the poll ends it.
    let mut lost = Task::new();
    send(&mut lost);
    lost.live(&[entry_frame(&user(1, "go"))]);
    assert_eq!(phase(&lost), Phase::Working);
    let step = BusyPolling::foreign().status(&status(false));
    assert!(step.finished);
    step.apply(&mut lost.state);
    assert_eq!(phase(&lost), Phase::Idle);
}

#[test]
fn only_a_reply_that_names_no_approval_and_another_reason_is_a_plain_turn_end() {
    let reply = |reason: &str| Ok(serde_json::from_value(json!({"stopReason": reason})).unwrap());
    for reason in ["end_turn", "cancelled", "max_steps", ""] {
        assert_eq!(
            prompt_outcome(&reply(reason)),
            PromptOutcome::Ended {
                events: vec![TaskEvent::TurnEnded],
                check_status: false
            },
            "{reason}"
        );
    }
}

#[test]
fn a_failure_after_the_post_may_have_been_sent_asks_for_the_status_and_one_before_does_not() {
    let io = |k: std::io::ErrorKind| Error::Io(k.into());
    let http = |status| Error::Status {
        status,
        message: String::new(),
        body: json!(null),
    };
    let decode = serde_json::from_str::<u8>("x").unwrap_err();
    let sent: Vec<Error> = vec![
        io(std::io::ErrorKind::TimedOut),
        io(std::io::ErrorKind::ConnectionReset),
        Error::Protocol("cut short".into()),
        Error::Decode(decode),
        http(408),
        http(409),
        http(500),
        http(502),
        http(503),
        http(504),
    ];
    for e in &sent {
        let PromptOutcome::Failed { check_status, .. } = prompt_outcome(&Err(match e {
            Error::Status { status, .. } => http(*status),
            Error::Io(x) => io(x.kind()),
            Error::Protocol(m) => Error::Protocol(m.clone()),
            _ => Error::Decode(serde_json::from_str::<u8>("x").unwrap_err()),
        })) else {
            panic!()
        };
        assert!(check_status, "{e}");
    }
    let unsent: Vec<Error> = vec![
        Error::Connect(std::io::ErrorKind::ConnectionRefused.into()),
        Error::InvalidUrl("x".into()),
        Error::InsecureTransport("x".into()),
        Error::InsecureSocket {
            path: "p".into(),
            reason: "r".into(),
        },
        Error::InvalidConfig("x".into()),
        Error::Tls("x".into()),
        http(400),
        http(401),
        http(404),
        http(429),
    ];
    for e in unsent {
        let text = e.to_string();
        let PromptOutcome::Failed {
            server_busy,
            check_status,
        } = prompt_outcome(&Err(e))
        else {
            panic!()
        };
        assert!(!check_status && !server_busy, "{text}");
    }
}

#[test]
fn the_retry_of_the_queue_fetch_backs_off_to_a_cap_and_stops() {
    let waits: Vec<_> = (0..MAX_APPROVALS_RETRIES)
        .map(|n| approvals_backoff(n).unwrap().as_secs())
        .collect();
    assert_eq!(waits, [2, 4, 8, 16, 32, 60, 60, 60]);
    assert_eq!(approvals_backoff(0), Some(APPROVALS_RETRY_BASE));
    assert_eq!(approvals_backoff(MAX_APPROVALS_RETRIES), None);
    assert_eq!(approvals_backoff(u32::MAX), None);
    assert!(waits.iter().all(|w| *w <= APPROVALS_RETRY_MAX.as_secs()));
    assert!(APPROVALS_RETRY_BASE >= Duration::from_secs(1));
}

fn requested(id: &str) -> StreamItem {
    approval_frame("approval_requested", id, "pending")
}

#[test]
fn live_approval_frames_are_bounded_by_the_mapper() {
    let mut t = Task::new();
    let flood: Vec<_> = (0..MAX_REFRESH_ROWS + 50)
        .map(|i| requested(&format!("a{i}")))
        .collect();
    t.live(&flood);
    assert_eq!(t.state.run.approvals.len(), MAX_REFRESH_ROWS);
    // A repeat of an open one passes (the model dedupes it); a new one is dropped.
    t.live(&[requested("a0"), requested("new")]);
    assert_eq!(t.state.run.approvals.len(), MAX_REFRESH_ROWS);
    assert!(t.state.run.approvals.iter().all(|a| a.id != "new"));
    // A resolution frees a slot, and a reconnect starts the count over.
    t.live(&[approval_frame("approval_resolved", "a1", "denied")]);
    t.live(&[requested("room")]);
    assert!(t.state.run.approvals.iter().any(|a| a.id == "room"));
    t.live(&[StreamItem::Reconnected { after: 0 }, requested("again")]);
    assert!(t.state.run.approvals.iter().any(|a| a.id == "again"));
}

#[test]
fn an_oversized_approval_id_is_not_admitted_and_a_decided_row_takes_no_slot() {
    let mut t = Task::new();
    t.live(&[requested(&"x".repeat(1000))]);
    assert!(t.state.run.approvals.is_empty());
    let mut m = EntryMapper::new();
    for i in 0..MAX_REFRESH_ROWS * 2 {
        let decided = approval_frame("approval_requested", &format!("d{i}"), "approved");
        assert_eq!(crate::adapter::task::stream_item(&mut m, &decided).len(), 2);
    }
    let open = crate::adapter::task::stream_item(&mut m, &requested("p"));
    assert_eq!(open.len(), 1);
}

// --- turn errors under the other user-role messages ---------------------------

fn turn_error(seq: u64, parent: &str) -> Entry {
    let payload = json!({"error": "cancelled", "detail": "stop"});
    entry(seq, &format!("e_{seq}"), Some(parent), "error", payload)
}

#[test]
fn a_turn_error_under_a_steer_note_or_a_stop_gate_message_settles_no_call() {
    let mut steer = user(2, "faster");
    steer.payload["interrupt"] = json!(true);
    steer.payload["source"] = json!("owner");
    let mut gate = user(4, "stop gate: nothing written");
    gate.payload["stop_gate_feedback"] = json!(true);
    let mut t = Task::new();
    // A payload with only `detail` would be caught by its shape; make it bare so
    // that only the remembered parent can tell.
    let bare = |seq: u64, parent: &str| {
        entry(
            seq,
            &format!("e_{seq}"),
            Some(parent),
            "error",
            json!({"error": "x"}),
        )
    };
    t.entries(&[
        user(1, "go"),
        steer,
        bare(3, "e_2"),
        gate,
        bare(5, "e_4"),
        turn_error(6, "e_4"),
    ]);
    assert_eq!(t.state.transcript.counts.unsure, 0);
}

#[test]
fn a_turn_error_while_an_intent_is_open_neither_settles_it_nor_loses_its_plan() {
    let mut t = Task::new();
    t.entries(&[
        user(1, "go"),
        intent(2, "write_file", json!({"path": "a.txt"})),
        intent(3, "todo_write", json!({})),
        turn_error(4, "e_1"),
    ]);
    // The write is still open: it counts as may-have-changed once the turn is over.
    assert_eq!(t.state.transcript.counts.unsure, 1);
    t.entries(&[result(5, 3, json!([{"content": "a", "status": "pending"}]))]);
    assert!(t
        .state
        .transcript
        .items
        .iter()
        .any(|i| matches!(i, Item::Plan(p) if p.len() == 1)));
    // Its own failure settles it, once.
    t.entries(&[failure(6, 2)]);
    assert_eq!(t.state.transcript.counts.unsure, 1);
}
