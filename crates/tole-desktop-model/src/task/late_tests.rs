//! Round 3 of the review: frames that arrive late, replayed history, and the
//! edges of the run state. Through the public [`TaskState`] only.

use super::*;
use crate::task_strings::status_text;

#[rustfmt::skip]
fn intent(seq: u64, tool: &str) -> TaskEvent {
    TaskEvent::ToolIntent { seq, id: format!("e{seq}"), tool: tool.into(), target: None }
}

#[rustfmt::skip]
fn answer(seq: u64, text: &str) -> TaskEvent { TaskEvent::AnswerEntry { seq, text: text.into() } }

#[rustfmt::skip]
fn delta(text: &str) -> TaskEvent { TaskEvent::TextDelta { text: text.into() } }

#[rustfmt::skip]
fn prompt(seq: u64) -> TaskEvent {
    TaskEvent::UserMessage { seq, text: "go".into(), images: 0, kind: UserKind::Prompt }
}

#[rustfmt::skip]
fn ask(id: &str) -> TaskEvent {
    TaskEvent::ApprovalRequested(ApprovalInfo { id: id.into(), tool: "write_file".into(), summary: None })
}

#[rustfmt::skip]
fn resolved(id: &str, approved: bool) -> TaskEvent { TaskEvent::ApprovalResolved { id: id.into(), approved } }

fn task(events: Vec<TaskEvent>) -> TaskState {
    let mut t = TaskState::new(false);
    events.into_iter().for_each(|e| t.apply(e));
    t
}

fn rows(t: &TaskState) -> Vec<String> {
    let row = |i: &Item| match i {
        Item::Answer {
            markdown,
            streaming,
        } => format!("{markdown}{}", if *streaming { "~" } else { "" }),
        other => format!("{other:?}").chars().take(4).collect(),
    };
    t.transcript.items.iter().map(row).collect()
}

fn sent(t: &mut TaskState) -> Submission {
    t.composer.text = "go".into();
    match t.press_enter(Enter {
        shift: false,
        composing: false,
    }) {
        EnterOutcome::Submit(s) => s,
        other => panic!("not sent: {other:?}"),
    }
}

// Major 1: text frames travel on SSE, the end of the turn on the prompt reply.
#[test]
fn a_frame_after_the_end_of_the_turn_or_its_answer_changes_nothing() {
    let end = TaskEvent::TurnEnded;
    let orders = [
        vec![delta("Hel"), end.clone(), delta("lo"), answer(5, "Hello.")],
        vec![delta("Hel"), answer(5, "Hello."), end.clone(), delta("x")],
        vec![delta("Hel"), answer(5, "Hello."), delta("x"), end.clone()],
        vec![end.clone(), delta("Hel"), answer(5, "Hello.")],
        vec![answer(5, "Hello."), end.clone(), delta("x")],
        vec![delta("Hel"), end.clone(), answer(5, "Hello."), delta("x")],
    ];
    for events in orders {
        let t = task(events.clone());
        assert_eq!(rows(&t), ["Hello."], "{events:?}");
        assert_eq!(t.status().phase, Phase::Idle, "{events:?}");
    }
}

#[test]
fn reasoning_after_the_end_of_a_turn_does_not_revive_it_and_a_new_prompt_does() {
    let t = task(vec![
        TaskEvent::Ready { busy: true },
        TaskEvent::TurnEnded,
        TaskEvent::Reasoning,
    ]);
    assert_eq!(t.status().phase, Phase::Idle);
    let mut t = t;
    sent(&mut t);
    t.apply(delta("again"));
    assert_eq!(rows(&t), ["again~"]);
}

// Major 2: a replayed plan belongs to an old turn.
#[test]
fn a_replayed_plan_does_not_change_the_current_turn() {
    let plan = |seq, text: &str| TaskEvent::PlanUpdated {
        seq,
        items: vec![PlanItem {
            text: text.into(),
            status: PlanStatus::Pending,
        }],
    };
    let history = vec![
        prompt(1),
        intent(2, "update_plan"),
        plan(2, "old"),
        answer(3, "a"),
        prompt(4),
    ];
    let mut t = task(history);
    let before = rows(&t);
    t.apply(plan(2, "old"));
    assert_eq!(rows(&t), before, "an old plan is no new row");
    t.apply(intent(5, "update_plan"));
    t.apply(plan(5, "new"));
    t.apply(plan(2, "old"));
    let plans: Vec<_> = t
        .transcript
        .items
        .iter()
        .filter_map(|i| match i {
            Item::Plan(p) => Some(p[0].text.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(plans, ["old", "new"]);
}

// Minors.
#[test]
fn a_not_busy_snapshot_freezes_the_draft_and_a_denied_approval_stops_the_spinner() {
    let t = task(vec![
        TaskEvent::Ready { busy: true },
        delta("abc"),
        TaskEvent::Ready { busy: false },
    ]);
    assert_eq!(rows(&t), ["abc"]);
    let t = task(vec![
        TaskEvent::Ready { busy: true },
        intent(1, "write_file"),
        ask("a1"),
        resolved("a1", false),
    ]);
    assert!(!t.group_active(0) && t.status().phase == Phase::Idle);
}

#[test]
fn a_refresh_neither_resurrects_a_decided_approval_nor_drops_an_announced_one() {
    let pending = |ids: &[&str]| TaskEvent::ApprovalsRefreshed {
        pending: ids
            .iter()
            .map(|id| ApprovalInfo {
                id: id.to_string(),
                tool: "t".into(),
                summary: None,
            })
            .collect(),
    };
    let ids = |t: &TaskState| {
        t.run
            .approvals
            .iter()
            .map(|a| a.id.clone())
            .collect::<Vec<_>>()
    };
    let decided = task(vec![
        TaskEvent::Reconnected,
        resolved("a2", true),
        pending(&["a2"]),
    ]);
    assert!(ids(&decided).is_empty());
    let first_load = task(vec![ask("a1"), pending(&[])]);
    assert_eq!(ids(&first_load), ["a1"]);
}

#[test]
fn a_failed_prompt_is_not_stopping_and_a_snapshot_cannot_reopen_the_composer_mid_send() {
    let mut t = TaskState::new(false);
    let s = sent(&mut t);
    assert!(t.press_stop());
    t.send_failed(s, false);
    assert_eq!(t.status().phase, Phase::Idle);
    let mut t = TaskState::new(false);
    sent(&mut t);
    t.apply(TaskEvent::Ready { busy: false });
    assert_eq!(
        t.status().send,
        SendMode::Steer,
        "the prompt is still going out"
    );
}

#[test]
fn an_unobserved_turn_can_still_time_out_after_its_first_tool_entry() {
    let events = vec![ask("a1"), resolved("a1", true), intent(1, "read_file")];
    let mut t = task(events);
    assert_eq!(t.status().phase, Phase::Working);
    t.apply(TaskEvent::ResumeTimedOut);
    assert_eq!(t.status().phase, Phase::Idle);
}

#[test]
fn restoring_a_send_cannot_pass_the_image_cap_and_names_are_printable() {
    let mut t = TaskState::new(false);
    for _ in 0..MAX_IMAGES {
        t.composer.attach("a.png", 5).unwrap();
    }
    let s = sent(&mut t);
    for _ in 0..MAX_IMAGES {
        t.composer.attach("b.png", 5).unwrap();
    }
    t.send_failed(s, false);
    assert_eq!(t.composer.attachments.len(), MAX_IMAGES);
    t.composer.attachments.clear();
    t.composer.attach("p\u{202E}g\nn.png", 5).unwrap();
    assert_eq!(t.composer.attachments[0].name, "pg n.png");
}

#[test]
fn the_window_survives_a_negative_or_nan_overscan() {
    let t = {
        let mut t = TaskState::new(false);
        (1..=100).for_each(|i| t.apply(prompt(i)));
        t
    };
    for overscan in [-1000.0, f32::NAN, f32::INFINITY] {
        let v = visible(&t.layout, 0.0, 100.0, overscan);
        assert!(v.start <= v.end && v.end <= 100, "{overscan}: {v:?}");
    }
}

#[test]
fn nothing_was_changed_is_only_said_for_a_complete_history() {
    let mut t = task(vec![prompt(1), answer(2, "x")]);
    assert_eq!(status_text(&t.status()), "Done. No changes seen.");
    t.history_complete = true;
    assert_eq!(status_text(&t.status()), "Done. Nothing was changed.");
}

// Round 4.
#[test]
fn a_not_busy_snapshot_right_after_an_allow_neither_ends_nor_silences_the_resumed_turn() {
    let allowed = || {
        vec![
            prompt(1),
            intent(2, "write_file"),
            ask("a1"),
            resolved("a1", true),
        ]
    };
    let mut t = task(allowed());
    t.apply(TaskEvent::Ready { busy: false });
    assert_eq!(
        (t.status().phase, t.status().can_stop),
        (Phase::Resuming, true)
    );
    t.apply(delta("Hi"));
    assert_eq!(rows(&t).last().unwrap(), "Hi~", "the text is kept");
    t.apply(answer(4, "Hi."));
    assert_eq!(rows(&t).iter().filter(|r| r.starts_with("Hi")).count(), 1);
    assert_eq!(t.status().phase, Phase::Idle);
    let mut quiet = task(allowed());
    quiet.apply(TaskEvent::Ready { busy: false });
    quiet.apply(TaskEvent::ResumeTimedOut);
    assert_eq!(quiet.status().phase, Phase::Idle);
    let mut stopped = task(allowed());
    assert!(stopped.press_stop());
    stopped.apply(TaskEvent::Ready { busy: false });
    assert_eq!(stopped.status().phase, Phase::Idle);
}

#[test]
fn an_approval_announced_after_it_was_decided_is_not_a_new_card() {
    for approved in [true, false] {
        let t = task(vec![
            prompt(1),
            ask("a1"),
            resolved("a1", approved),
            ask("a1"),
        ]);
        assert!(t.run.approvals.is_empty(), "approved: {approved}");
        assert_ne!(t.status().phase, Phase::WaitingApproval);
    }
}

#[test]
fn stop_while_the_prompt_goes_out_ends_with_the_not_busy_answer_to_cancel() {
    let mut t = TaskState::new(false);
    sent(&mut t);
    assert!(t.press_stop());
    t.apply(TaskEvent::Ready { busy: false });
    assert_eq!(t.status().phase, Phase::Idle);
}
