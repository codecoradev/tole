//! Malformed and hostile server data: nothing panics, nothing invents state.

use super::*;
use crate::adapter::task::{approvals_refreshed, prompt_outcome, stream_result, PromptOutcome};
use tole_client::Error;
use tole_desktop_model::task::{Phase, PlanItem, PlanStatus, UserKind};

fn mapped(e: &Entry) -> Vec<TaskEvent> {
    EntryMapper::new().entry(e)
}

#[test]
fn payloads_of_the_wrong_shape_map_to_nothing_or_the_careful_event() {
    for payload in [
        json!(null),
        json!("text"),
        json!([1, 2]),
        json!(7),
        json!({}),
    ] {
        for kind in ["message", "intent", "tool_result", "error", "usage", ""] {
            let e = entry(1, "e", Some("p"), kind, payload.clone());
            let events = mapped(&e);
            // Only a settlement with a parent says anything about a bad payload.
            let settles = matches!(kind, "tool_result" | "error");
            assert_eq!(events.len(), usize::from(settles), "{kind} {payload}");
        }
    }
}

#[test]
fn a_message_with_odd_fields_keeps_the_turn_structure_but_invents_no_answer() {
    let odd = |payload: Value| mapped(&entry(1, "e", None, "message", payload));
    let user = odd(json!({"role": "user", "text": 5, "images": "many"}));
    assert_eq!(
        user,
        [TaskEvent::UserMessage {
            seq: 1,
            text: String::new(),
            images: 0,
            kind: UserKind::Prompt
        }]
    );
    assert!(odd(json!({"role": "assistant"})).is_empty());
    assert!(odd(json!({"role": "assistant", "text": ["x"]})).is_empty());
    assert!(odd(json!({"role": 1, "text": "x"})).is_empty());
    assert!(odd(json!({"role": "system", "text": "x"})).is_empty());
    assert!(odd(json!({"text": "x"})).is_empty());
}

#[test]
fn an_intent_without_a_tool_is_dropped_and_its_settlement_counts_as_unsure() {
    let mut t = Task::new();
    let nameless = entry(2, "i2", None, "intent", json!({"input": {"path": "a"}}));
    t.entries(&[user(1, "go"), nameless, result(3, 2, json!("x"))]);
    assert_eq!(t.shape(), "U");
    assert_eq!(t.state.transcript.counts.unsure, 1);
}

#[test]
fn a_settlement_without_a_parent_or_for_an_unknown_one_is_harmless() {
    let orphan = entry(2, "r2", None, "tool_result", json!({"ok": true}));
    assert!(mapped(&orphan).is_empty());
    // A todo_write result the mapper never saw the intent of publishes no plan.
    let lone = result(3, 2, json!([{"content": "a"}]));
    assert!(matches!(
        &mapped(&lone)[..],
        [TaskEvent::ToolSettled { .. }]
    ));
}

#[test]
fn a_todo_write_error_publishes_no_plan_and_forgets_the_intent() {
    let mut m = EntryMapper::new();
    m.entry(&intent(2, "todo_write", json!({})));
    let events = m.entry(&failure(3, 2));
    assert!(matches!(
        &events[..],
        [TaskEvent::ToolSettled { ok: false, .. }]
    ));
    // The intent is gone: a second settlement of it is not a plan either.
    let again = m.entry(&result(4, 2, json!([{"content": "a"}])));
    assert_eq!(again.len(), 1);
}

#[test]
fn a_plan_list_is_read_leniently_from_a_result_and_bounded() {
    let mut m = EntryMapper::new();
    m.entry(&intent(1, "todo_write", json!({})));
    let big: Vec<Value> = (0..5000)
        .map(|i| json!({"content": format!("t{i}"), "status": "weird"}))
        .chain([json!({"status": "completed"}), json!(3), json!(null)])
        .collect();
    let events = m.entry(&result(2, 1, Value::Array(big)));
    let [TaskEvent::ToolSettled { .. }, TaskEvent::PlanUpdated { seq: 2, items }] = &events[..]
    else {
        panic!("{events:?}")
    };
    assert_eq!(items.len(), 200);
    assert_eq!(
        items[0],
        PlanItem {
            text: "t0".into(),
            status: PlanStatus::Pending
        }
    );
    // An output that is not a list is not a plan.
    m.entry(&intent(3, "todo_write", json!({})));
    assert_eq!(m.entry(&result(4, 3, json!({"content": "a"}))).len(), 1);
}

#[test]
fn the_open_todo_intents_are_bounded_and_the_oldest_is_forgotten() {
    let mut m = EntryMapper::new();
    for i in 0..40u64 {
        m.entry(&intent(i * 2 + 1, "todo_write", json!({})));
    }
    // Intent 1 fell out of the window, the newest is still there.
    assert_eq!(m.entry(&result(100, 1, json!([{"content": "a"}]))).len(), 1);
    assert_eq!(
        m.entry(&result(101, 79, json!([{"content": "a"}]))).len(),
        2
    );
}

#[test]
fn a_huge_id_or_seq_or_image_list_does_not_panic_or_wrap() {
    let long = "x".repeat(10_000);
    let mut m = EntryMapper::new();
    let e = entry(
        u64::MAX,
        &long,
        None,
        "intent",
        json!({"tool": "todo_write"}),
    );
    assert!(matches!(
        &m.entry(&e)[..],
        [TaskEvent::ToolIntent { seq: u64::MAX, .. }]
    ));
    let settle = entry(
        u64::MAX,
        "r",
        Some(&long),
        "tool_result",
        json!({"output": [{"content": "a"}]}),
    );
    assert_eq!(
        m.entry(&settle).len(),
        1,
        "an id that long is not remembered"
    );
    let mut u = user(u64::MAX, "go");
    u.payload["images"] = Value::Array(vec![json!(0); 100_000]);
    assert!(matches!(
        &m.entry(&u)[..],
        [TaskEvent::UserMessage {
            images: 100_000,
            ..
        }]
    ));
}

#[test]
fn text_is_passed_on_raw_and_the_model_makes_it_printable() {
    let raw = "a\u{202E}b\u{0}\nc";
    let mut t = Task::new();
    t.live(&[ready(true), delta(raw)]);
    t.entries(&[
        user(1, "go"),
        intent(2, "run_command", json!({"command": raw})),
        answer(3, raw),
    ]);
    let events = mapped(&answer(1, raw));
    assert!(matches!(&events[..], [TaskEvent::AnswerEntry { text, .. }] if text == raw));
    let Some(Item::Answer { markdown, .. }) = t.state.transcript.items.last() else {
        panic!()
    };
    assert!(!markdown.contains('\u{202E}') && !markdown.contains('\0') && markdown.contains('\n'));
}

#[test]
fn unknown_and_reshaped_frames_say_nothing_and_a_broken_entry_frame_loses_the_stream() {
    let mut m = EntryMapper::new();
    let unknown = frame("shiny_new_frame", json!({"x": 1}));
    assert!(stream_result(&mut m, &Ok(Some(unknown))).is_empty());
    // A known ephemeral frame of a changed shape decodes to `Other`.
    let reshaped = frame("text_delta", json!({"txt": 1}));
    assert!(stream_result(&mut m, &Ok(Some(reshaped))).is_empty());
    let not_json = Frame {
        id: None,
        event: "approval_requested".into(),
        data: "{{".into(),
    };
    let other = StreamItem::Event(Event::from_frame(&not_json).unwrap());
    assert!(stream_result(&mut m, &Ok(Some(other))).is_empty());
    // A durable entry that does not decode is a client error: the stream is fused.
    let broken = Frame {
        id: Some(1),
        event: "entry".into(),
        data: "{}".into(),
    };
    let err = Event::from_frame(&broken).unwrap_err();
    assert_eq!(
        stream_result(&mut m, &Err(err)),
        [TaskEvent::ConnectionLost]
    );
    let framing = Error::Protocol("line too long".into());
    assert_eq!(
        stream_result(&mut m, &Err(framing)),
        [TaskEvent::ConnectionLost]
    );
}

#[test]
fn an_approval_that_is_not_pending_is_not_a_card_and_a_resolved_one_allows_only_on_approved() {
    let mut t = Task::new();
    t.live(&[approval_frame("approval_requested", "a1", "approved")]);
    assert!(t.state.run.approvals.is_empty(), "the card is gone at once");
    // An unknown status is shown: the server has the last word.
    t.live(&[approval_frame("approval_requested", "a2", "queued")]);
    assert_eq!(t.state.run.approvals.len(), 1);
    // A resolved frame that says pending (or anything else) allows nothing.
    t.live(&[approval_frame("approval_resolved", "a2", "pending")]);
    assert_eq!(t.state.status().phase, Phase::Idle);
    for (status, approved) in [
        ("approved", true),
        ("denied", false),
        ("expired", false),
        ("", false),
    ] {
        let events = stream_result(
            &mut t.mapper,
            &Ok(Some(approval_frame("approval_resolved", "x", status))),
        );
        assert_eq!(
            events,
            [TaskEvent::ApprovalResolved {
                id: "x".into(),
                approved
            }]
        );
    }
}

#[test]
fn the_summary_is_the_description_else_the_target_and_hostile_text_is_cleaned_by_the_model() {
    let row = |extra: Value| {
        let mut r = json!({"id": "a1", "tool": "write_file\u{202E}", "status": "pending"});
        r.as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        r
    };
    let summary = json!({"kind": "create_file", "target": "src/a.rs", "effect": "adds"});
    let cases = [
        (
            row(json!({"description": "write a", "summary": summary})),
            Some("write a"),
        ),
        (
            row(json!({"description": "", "summary": summary})),
            Some("src/a.rs"),
        ),
        (row(json!({"summary": summary})), Some("src/a.rs")),
        (row(json!({})), None),
    ];
    for (r, want) in cases {
        let a = serde_json::from_value(r).unwrap();
        let info = crate::adapter::task::approval_info(&a);
        assert_eq!(info.summary.as_deref(), want);
    }
    let mut t = Task::new();
    t.live(&[frame(
        "approval_requested",
        row(json!({"description": "a\nb"})),
    )]);
    assert_eq!(t.state.run.approvals[0].tool, "write_file");
    assert_eq!(t.state.run.approvals[0].summary.as_deref(), Some("a b"));
}

#[test]
fn a_queue_with_nothing_for_this_task_is_an_empty_refresh() {
    let foreign = serde_json::from_value(
        json!({"id": "a", "tool": "t", "status": "pending", "sessionId": "other"}),
    )
    .unwrap();
    let event = approvals_refreshed(&[foreign], "s1");
    assert_eq!(
        event,
        Some(TaskEvent::ApprovalsRefreshed { pending: vec![] })
    );
}

#[test]
fn a_reply_with_an_unknown_stop_reason_still_ends_the_turn() {
    let reply = serde_json::from_value(json!({"stopReason": "something_new"})).unwrap();
    assert_eq!(
        prompt_outcome(&Ok(reply)),
        PromptOutcome::Ended {
            events: vec![TaskEvent::TurnEnded],
            check_status: false
        }
    );
}
