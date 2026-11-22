//! A live task: deltas, approvals, an allow, a reconnect, a stop.

use super::*;
use crate::adapter::task::{
    approvals_refreshed, prompt_outcome, status_ready, stop_step, stream_result, PromptOutcome,
    ResumePolling, StopStep, RESUME_TIMEOUT,
};
use std::time::Duration;
use tole_client::{CancelReply, Error, PromptReply, SessionStatus};
use tole_desktop_model::task::{EnterOutcome, Link, Phase};

fn send(t: &mut Task, text: &str) {
    t.state.composer.text = text.into();
    let enter = tole_desktop_model::task::Enter {
        shift: false,
        composing: false,
    };
    assert!(matches!(
        t.state.press_enter(enter),
        EnterOutcome::Submit(_)
    ));
}

fn reply(approval: Option<Value>) -> Result<PromptReply, Error> {
    let mut row = json!({"stopReason": "end_turn", "text": "hi"});
    if let Some(a) = approval {
        row["stopReason"] = json!("approval_required");
        row["approval"] = a;
    }
    Ok(serde_json::from_value(row).expect("a reply"))
}

fn status(busy: bool) -> Result<SessionStatus, Error> {
    let row = json!({"id": "s1", "entries": null, "busy": busy, "usage_report": {},
        "model": null, "effort": null, "contextWindow": null, "contextUsed": null,
        "name": null, "snapshotAt": null, "stale": false});
    Ok(serde_json::from_value(row).expect("a status"))
}

fn cancel(cancelled: bool) -> Result<CancelReply, Error> {
    let row = json!({"cancelled": cancelled, "sessionId": "s1"});
    Ok(serde_json::from_value(row).expect("a cancel reply"))
}

fn phase(t: &Task) -> Phase {
    t.state.status().phase
}

#[test]
fn a_live_turn_streams_a_draft_which_the_answer_entry_replaces_and_the_reply_ends_the_turn() {
    let mut t = Task::new();
    send(&mut t, "tidy");
    assert_eq!(phase(&t), Phase::Working);
    t.live(&[
        ready(true),
        entry_frame(&user(1, "tidy")),
        frame("reasoning", json!({"text": "hmm"})),
        delta("Hel"),
        delta("lo"),
    ]);
    assert_eq!(t.shape(), "U a");
    t.live(&[
        entry_frame(&intent(2, "read_file", json!({"path": "a.rs"}))),
        frame("tool_started", json!({"tool": "read_file", "input": {}})),
        frame(
            "tool_finished",
            json!({"tool": "read_file", "ok": true, "preview": ""}),
        ),
        entry_frame(&result(3, 2, json!("x"))),
        StreamItem::Heartbeat,
        entry_frame(&answer(4, "Hello.")),
    ]);
    assert_eq!(t.shape(), "U A W1 A", "the draft a tool followed is a note");
    // The reply of the prompt call comes after the answer entry.
    let PromptOutcome::Ended { events, .. } = prompt_outcome(&reply(None)) else {
        panic!()
    };
    assert_eq!(events, [TaskEvent::TurnEnded]);
    t.apply(events);
    assert_eq!(phase(&t), Phase::Idle);
    // A late delta of the finished turn is not text of a new one.
    t.live(&[delta("late")]);
    assert_eq!(t.shape(), "U A W1 A");
}

#[test]
fn a_turn_that_asks_waits_and_an_allow_resumes_it_until_the_snapshot_and_the_answer() {
    let mut t = Task::new();
    send(&mut t, "write it");
    t.live(&[
        entry_frame(&user(1, "write it")),
        entry_frame(&intent(2, "write_file", json!({"path": "a.txt"}))),
        approval_frame("approval_requested", "apr-1", "pending"),
    ]);
    assert_eq!(phase(&t), Phase::WaitingApproval);
    // The prompt returns with the same row: one card, and the turn is over.
    let row = json!({"id": "apr-1", "tool": "write_file", "status": "pending", "description": "write a.txt"});
    let PromptOutcome::Ended { events, .. } = prompt_outcome(&reply(Some(row))) else {
        panic!()
    };
    assert_eq!(events.len(), 2);
    t.apply(events);
    assert_eq!(t.state.run.approvals.len(), 1);
    // The first announcement of an id is the one the model keeps.
    assert_eq!(t.state.run.approvals[0].summary, None);
    t.live(&[approval_frame("approval_resolved", "apr-1", "approved")]);
    assert_eq!(phase(&t), Phase::Resuming);

    let mut poll = ResumePolling::new();
    let mut at = Duration::from_millis(500);
    // serve answers 202 before it starts the turn: not busy yet is normal.
    let step = poll.status(&status(false), at);
    assert!(!step.finished);
    t.apply(step.events);
    assert_eq!(phase(&t), Phase::Resuming);
    at += Duration::from_millis(500);
    let step = poll.status(&status(true), at);
    t.apply(step.events);
    assert_eq!(phase(&t), Phase::Working);
    t.live(&[
        entry_frame(&result(3, 2, json!({"written": 1}))),
        entry_frame(&answer(4, "Written.")),
    ]);
    assert_eq!(phase(&t), Phase::Idle);
    assert_eq!(t.state.transcript.counts.changed, 1);
}

#[test]
fn an_allowed_turn_that_never_shows_up_times_out_and_a_denial_ends_the_parked_turn() {
    let mut t = Task::new();
    send(&mut t, "go");
    t.live(&[
        approval_frame("approval_requested", "a1", "pending"),
        approval_frame("approval_resolved", "a1", "approved"),
    ]);
    let mut poll = ResumePolling::new();
    let step = poll.status(&status(false), RESUME_TIMEOUT);
    assert!(step.finished);
    t.apply(step.events);
    assert_eq!(phase(&t), Phase::Idle);

    send(&mut t, "again");
    t.live(&[
        approval_frame("approval_requested", "a2", "pending"),
        approval_frame("approval_resolved", "a2", "denied"),
    ]);
    assert_eq!(phase(&t), Phase::Idle);
}

#[test]
fn a_reconnect_drops_the_draft_refreshes_the_queue_and_takes_the_answer_from_its_entry() {
    let mut t = Task::new();
    t.live(&[ready(true), delta("Hel")]);
    t.live(&[StreamItem::Reconnected { after: 1 }, delta("lo")]);
    assert!(t.state.run.approvals_stale);
    assert_eq!(t.shape(), "", "no stale text");
    // The driver fetched the queue: one pending of this task, one decided, one foreign.
    let rows = vec![
        approval("a1", "pending"),
        approval("a2", "approved"),
        serde_json::from_value(
            json!({"id": "a3", "tool": "x", "status": "pending", "sessionId": "other"}),
        )
        .unwrap(),
    ];
    t.apply(approvals_refreshed(&rows, "s1").into_iter().collect());
    assert!(!t.state.run.approvals_stale);
    assert_eq!(phase(&t), Phase::WaitingApproval);
    let ids: Vec<_> = t
        .state
        .run
        .approvals
        .iter()
        .map(|a| a.id.as_str())
        .collect();
    assert_eq!(ids, ["a1"]);
    t.live(&[entry_frame(&answer(5, "Hello."))]);
    assert_eq!(t.shape(), "A");
}

#[test]
fn resync_and_closed_frames_and_a_lost_stream() {
    let mut t = Task::new();
    t.live(&[
        ready(true),
        delta("a"),
        frame("resync", json!({"dropped": 3})),
        delta("b"),
    ]);
    assert_eq!(t.shape(), "", "text after a resync is stale");
    let lost = stream_result(&mut t.mapper, &Err(Error::ReconnectExhausted(8)));
    assert_eq!(lost, [TaskEvent::ConnectionLost]);
    t.apply(lost);
    assert_eq!((phase(&t), t.state.run.link), (Phase::Offline, Link::Lost));
    t.live(&[ready(true)]);
    assert_eq!(phase(&t), Phase::Working, "a frame proves the link");
    t.live(&[frame("closed", json!({"id": "s1"}))]);
    assert_eq!(phase(&t), Phase::Closed);
    assert!(stream_result(&mut t.mapper, &Ok(None)).is_empty());
}

#[test]
fn a_refusal_of_the_stream_is_a_lost_connection_and_the_snapshot_is_ready() {
    let mut mapper = EntryMapper::new();
    let refused = Error::Status {
        status: 401,
        message: "no".into(),
        body: json!(null),
    };
    assert_eq!(
        stream_result(&mut mapper, &Err(refused)),
        [TaskEvent::ConnectionLost]
    );
    let busy = status(true).unwrap();
    assert_eq!(status_ready(&busy), TaskEvent::Ready { busy: true });
}

#[test]
fn stop_asks_once_and_each_answer_lands_in_the_state() {
    let mut t = Task::new();
    t.live(&[ready(true)]);
    assert!(t.state.press_stop() && !t.state.press_stop());
    assert_eq!(phase(&t), Phase::Stopping);
    // The call failed: the turn runs on.
    stop_step(&Err(Error::Protocol("x".into()))).apply(&mut t.state);
    assert_eq!(phase(&t), Phase::Working);
    // Cancelled: wait for the end; the reply of the prompt ends it.
    assert!(t.state.press_stop());
    let step = stop_step(&cancel(true));
    assert_eq!(step, StopStep::Wait);
    step.apply(&mut t.state);
    assert_eq!(phase(&t), Phase::Stopping);
    t.apply(vec![TaskEvent::TurnEnded]);
    assert_eq!(phase(&t), Phase::Idle);
    // Nothing was running on the server after all.
    t.live(&[ready(true)]);
    assert!(t.state.press_stop());
    stop_step(&cancel(false)).apply(&mut t.state);
    assert_eq!(phase(&t), Phase::Idle);
}

#[test]
fn a_stop_while_the_prompt_goes_out_is_honoured_when_the_server_says_idle() {
    let mut t = Task::new();
    send(&mut t, "go");
    assert!(t.state.press_stop());
    stop_step(&cancel(false)).apply(&mut t.state);
    assert_eq!(phase(&t), Phase::Idle);
}

#[test]
fn a_failed_prompt_asks_for_the_status_only_when_the_turn_may_run() {
    let status = |e: Error| match prompt_outcome(&Err(e)) {
        PromptOutcome::Failed {
            server_busy,
            check_status,
        } => (server_busy, check_status),
        other => panic!("{other:?}"),
    };
    let http = |status| Error::Status {
        status,
        message: String::new(),
        body: json!(null),
    };
    assert_eq!(status(http(409)), (true, true));
    assert_eq!(status(http(500)), (false, true));
    assert_eq!(status(http(401)), (false, false));
    assert_eq!(
        status(Error::Connect(std::io::ErrorKind::ConnectionRefused.into())),
        (false, false)
    );
    assert_eq!(
        status(Error::Io(std::io::ErrorKind::TimedOut.into())),
        (false, true)
    );
}

#[test]
fn a_reply_row_that_was_decided_meanwhile_is_a_card_that_the_resolution_removes_after_the_turn_end()
{
    for (status, approved) in [("approved", true), ("denied", false), ("expired", false)] {
        let row = json!({"id": "a", "tool": "write_file", "status": status});
        let PromptOutcome::Ended { events, .. } = prompt_outcome(&reply(Some(row))) else {
            panic!()
        };
        assert_eq!(events.len(), 3, "{status}");
        assert_eq!(events[1], TaskEvent::TurnEnded);
        assert_eq!(
            events[2],
            TaskEvent::ApprovalResolved {
                id: "a".into(),
                approved
            }
        );
        let mut t = Task::new();
        send(&mut t, "go");
        t.apply(events);
        // An allow resumed the turn after the reply; a decline leaves it idle.
        let want = if approved {
            Phase::Resuming
        } else {
            Phase::Idle
        };
        assert_eq!(phase(&t), want, "{status}");
    }
}
