//! One test per review finding, through the public [`TaskState`] only.

use super::*;
use crate::task_strings::{status_text, step_label};

#[rustfmt::skip]
fn intent(seq: u64, tool: &str, target: Option<&str>) -> TaskEvent {
    TaskEvent::ToolIntent { seq, id: format!("e{seq}"), tool: tool.into(), target: target.map(String::from) }
}

#[rustfmt::skip]
fn settled(seq: u64, of: u64, ok: bool) -> TaskEvent {
    TaskEvent::ToolSettled { seq, intent_id: format!("e{of}"), ok }
}

#[rustfmt::skip]
fn answer(seq: u64, text: &str) -> TaskEvent { TaskEvent::AnswerEntry { seq, text: text.into() } }

#[rustfmt::skip]
fn delta(text: &str) -> TaskEvent { TaskEvent::TextDelta { text: text.into() } }

#[rustfmt::skip]
fn ask(id: &str, tool: &str) -> TaskEvent {
    TaskEvent::ApprovalRequested(ApprovalInfo { id: id.into(), tool: tool.into(), summary: None })
}

#[rustfmt::skip]
fn resolved(id: &str, approved: bool) -> TaskEvent { TaskEvent::ApprovalResolved { id: id.into(), approved } }

fn task(events: Vec<TaskEvent>) -> TaskState {
    let mut t = TaskState::new(false);
    events.into_iter().for_each(|e| t.apply(e));
    t
}

fn answers(t: &TaskState) -> Vec<String> {
    let all = t.transcript.items.iter();
    all.filter_map(|i| match i {
        Item::Answer { markdown, .. } => Some(markdown.clone()),
        _ => None,
    })
    .collect()
}

// Review 1: the prompt reply (TurnEnded) usually beats the answer entry.
#[test]
fn the_answer_entry_replaces_the_draft_in_every_order() {
    let orders = [
        vec![delta("Hel"), TaskEvent::TurnEnded, answer(5, "Hello.")],
        vec![delta("Hel"), answer(5, "Hello."), TaskEvent::TurnEnded],
        vec![
            delta("Hel"),
            TaskEvent::Reconnected,
            TaskEvent::TurnEnded,
            answer(5, "Hello."),
        ],
        vec![
            delta("Hel"),
            TaskEvent::TurnEnded,
            TaskEvent::Reconnected,
            answer(5, "Hello."),
        ],
    ];
    for events in orders {
        assert_eq!(answers(&task(events.clone())), ["Hello."], "{events:?}");
    }
}

// Review 2: a write that did not settle cleanly is never "nothing changed".
#[test]
fn nothing_was_changed_is_not_said_after_a_write_that_did_not_settle_cleanly() {
    let cases = [
        vec![
            intent(1, "write_file", None),
            settled(2, 1, false),
            answer(3, "x"),
        ],
        vec![intent(1, "write_file", None), TaskEvent::TurnEnded],
        vec![
            intent(1, "write_file", None),
            settled(5, 9, true),
            answer(6, "x"),
        ],
        vec![intent(1, "write_file", None), answer(3, "x")],
    ];
    for events in cases {
        let said = status_text(&task(events.clone()).status());
        assert!(
            said.contains("may have been changed"),
            "{said} for {events:?}"
        );
    }
    let clean = [
        intent(1, "read_file", None),
        settled(2, 1, true),
        answer(3, "x"),
    ];
    assert_eq!(
        status_text(&task(clean.to_vec()).status()),
        "Done. Nothing was changed."
    );
}

// Review 3: nothing a model wrote reaches the screen raw.
#[test]
fn model_text_is_made_printable_and_cut_before_it_is_kept() {
    let evil = "a\n\u{1b}[31m\u{202E}b";
    let long = "x".repeat(10_000);
    let t = task(vec![
        intent(1, "write_file", Some(evil)),
        intent(2, "mcp_x\u{202E}", Some(&long)),
    ]);
    let Item::Work(g) = &t.transcript.items[0] else {
        panic!()
    };
    for s in &g.steps {
        let shown = step_label(s);
        assert!(
            !shown.chars().any(|c| c.is_control() || c == '\u{202E}'),
            "{shown:?}"
        );
        assert!(shown.chars().count() < 300);
    }
    assert!(g.steps[1].target.as_deref().unwrap().ends_with('\u{2026}'));
}

// Review 4: a turn that parks on a second approval, then is denied, is idle.
#[test]
fn a_denial_after_an_allow_leaves_the_task_idle_and_the_composer_usable() {
    let mut t = task(vec![
        ask("a1", "write_file"),
        resolved("a1", true),
        ask("a2", "run_command"),
    ]);
    assert_eq!(t.status().phase, Phase::WaitingApproval);
    t.apply(resolved("a2", false));
    assert_eq!(t.status().phase, Phase::Idle);
    assert_eq!(t.status().send, SendMode::Send);
}

// Minor findings.
#[test]
fn an_old_answer_entry_does_not_end_a_running_turn() {
    let mut t = task(vec![answer(5, "a"), TaskEvent::Ready { busy: true }]);
    t.apply(answer(5, "a"));
    assert_eq!(t.status().phase, Phase::Working);
}

#[test]
fn a_step_left_running_by_a_cancelled_turn_is_shown_as_unfinished() {
    let t = task(vec![
        TaskEvent::Ready { busy: true },
        intent(1, "read_file", None),
        TaskEvent::TurnEnded,
    ]);
    let Item::Work(g) = &t.transcript.items[0] else {
        panic!()
    };
    assert_eq!(g.steps[0].state, StepState::Unfinished);
    assert!(!t.group_active(0));
}

#[test]
fn a_refused_prompt_stays_working_when_serve_said_busy() {
    let mut t = TaskState::new(false);
    t.composer.text = "go".into();
    let EnterOutcome::Submit(sent) = t.press_enter(Enter {
        shift: false,
        composing: false,
    }) else {
        panic!()
    };
    t.send_failed(sent, true);
    assert_eq!(
        (t.status().phase, t.composer.text.as_str()),
        (Phase::Working, "go")
    );
}
