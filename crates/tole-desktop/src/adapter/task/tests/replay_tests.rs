//! A history replay: entries (pages and live) to rows.

use super::*;
use crate::adapter::task::{history_page, PageStep};
use tole_client::EntriesPage;
use tole_desktop_model::task::{Phase, PlanItem, PlanStatus, UserKind};

fn plan(text: &str, status: PlanStatus) -> PlanItem {
    PlanItem {
        text: text.into(),
        status,
    }
}

fn page(entries: Vec<Entry>, next_after: u64, has_more: bool) -> EntriesPage {
    let entries = serde_json::to_value(entries).unwrap();
    let row = json!({"id": "s1", "entries": entries, "nextAfter": next_after,
        "hasMore": has_more, "lastSeq": if has_more { 99 } else { next_after }});
    serde_json::from_value(row).expect("a page")
}

#[test]
fn a_history_becomes_rows_and_the_plan_of_the_turn_is_the_last_list() {
    let mut t = Task::new();
    let mut prompt = user(1, "tidy up");
    prompt.payload["images"] = json!([{"id": "a"}, {"id": "b"}]);
    let plan_input = json!({"entries": [
        {"content": "look", "status": "in_progress"},
        {"content": "fix", "priority": "high"}]});
    let todo = json!([
        {"id": "t1", "content": "look", "status": "completed"},
        {"id": "t2", "content": "fix", "status": "in_progress"}]);
    t.entries(&[
        prompt,
        intent(2, "update_plan", plan_input),
        result(3, 2, json!({"ok": true})),
        intent(4, "read_file", json!({"path": "src/lib.rs"})),
        result(5, 4, json!("fn main() {}")),
        intent(
            6,
            "write_file",
            json!({"path": "src/lib.rs", "content": "x"}),
        ),
        result(7, 6, json!({"written": 1})),
        intent(8, "todo_write", json!({"todos": [{"content": "ignored"}]})),
        result(9, 8, todo),
        answer(10, "Done."),
    ]);
    assert_eq!(t.shape(), "U P W2 A");
    let items = &t.state.transcript.items;
    assert!(matches!(&items[0], Item::User { text, images: 2, steer: false } if text == "tidy up"));
    let want = [
        plan("look", PlanStatus::Completed),
        plan("fix", PlanStatus::InProgress),
    ];
    assert!(matches!(&items[1], Item::Plan(p) if p[..] == want[..]));
    let Item::Work(group) = &items[2] else {
        panic!("{items:?}")
    };
    assert_eq!(group.steps[0].target.as_deref(), Some("src/lib.rs"));
    assert_eq!(group.steps[1].id, "i6");
    let counts = t.state.transcript.counts;
    assert_eq!((counts.read, counts.changed, counts.unsure), (1, 1, 0));
    // History alone never makes the task busy.
    assert_eq!(t.state.status().phase, Phase::Idle);
}

#[test]
fn an_update_plan_that_core_refuses_publishes_no_plan() {
    let mut t = Task::new();
    let bad_status = json!({"entries": [{"content": "x", "status": "someday"}]});
    let no_content = json!({"entries": [{"content": "  "}]});
    t.entries(&[
        user(1, "go"),
        intent(2, "update_plan", bad_status),
        failure(3, 2),
        intent(4, "update_plan", no_content),
        failure(5, 4),
        intent(6, "update_plan", json!({"entries": "nope"})),
    ]);
    assert_eq!(t.shape(), "U");
}

#[test]
fn a_command_is_the_target_when_there_is_no_path_and_an_empty_one_is_none() {
    let mut mapper = EntryMapper::new();
    let target =
        |m: &mut EntryMapper, input: Value| match &m.entry(&intent(2, "run_command", input))[0] {
            TaskEvent::ToolIntent { target, .. } => target.clone(),
            other => panic!("{other:?}"),
        };
    assert_eq!(
        target(&mut mapper, json!({"command": "ls -la"})).as_deref(),
        Some("ls -la")
    );
    assert_eq!(
        target(&mut mapper, json!({"path": "a", "command": "b"})).as_deref(),
        Some("a")
    );
    assert_eq!(
        target(&mut mapper, json!({"path": "", "command": "b"})).as_deref(),
        Some("b")
    );
    assert_eq!(target(&mut mapper, json!({"path": 5})), None);
    assert_eq!(target(&mut mapper, json!(null)), None);
}

#[test]
fn the_flags_of_a_user_message_decide_its_kind_like_core_reads_them() {
    let kind = |flags: Value| {
        let mut e = user(1, "x");
        for (k, v) in flags.as_object().unwrap() {
            e.payload[k] = v.clone();
        }
        match &EntryMapper::new().entry(&e)[..] {
            [TaskEvent::UserMessage { kind, .. }] => *kind,
            other => panic!("{other:?}"),
        }
    };
    assert_eq!(kind(json!({})), UserKind::Prompt);
    assert_eq!(
        kind(json!({"stop_gate_feedback": true})),
        UserKind::Synthetic
    );
    assert_eq!(kind(json!({"denial_note": true})), UserKind::Synthetic);
    assert_eq!(kind(json!({"interrupt": true})), UserKind::Steer);
    // Tole's own words are never the owner's, even beside the steer flag.
    assert_eq!(
        kind(json!({"interrupt": true, "denial_note": true})),
        UserKind::Synthetic
    );
    // A flag counts only as the JSON `true`.
    assert_eq!(
        kind(json!({"interrupt": "true", "denial_note": 1})),
        UserKind::Prompt
    );
    assert_eq!(kind(json!({"stop_gate_feedback": false})), UserKind::Prompt);
}

#[test]
fn notes_and_steers_open_no_turn_in_the_rows() {
    let mut t = Task::new();
    let mut steer = user(3, "faster");
    steer.payload["interrupt"] = json!(true);
    let mut note = user(4, "[tole] denied");
    note.payload["denial_note"] = json!(true);
    t.entries(&[user(1, "go"), answer(2, "ok"), steer, note]);
    assert_eq!(t.shape(), "U A s N");
}

#[test]
fn pages_feed_one_mapper_and_say_when_the_history_is_whole() {
    let mut mapper = EntryMapper::new();
    let mut state = TaskState::new(false);
    let first = page(
        vec![user(1, "go"), intent(2, "todo_write", json!({"todos": []}))],
        2,
        true,
    );
    let second = page(
        vec![
            result(3, 2, json!([{"content": "a", "status": "pending"}])),
            answer(4, "ok"),
        ],
        4,
        false,
    );
    let step = |mapper: &mut EntryMapper, state: &mut TaskState, p: &EntriesPage, after| {
        let h = history_page(mapper, p, after);
        h.events.into_iter().for_each(|e| state.apply(e));
        h.step
    };
    assert_eq!(
        step(&mut mapper, &mut state, &first, 0),
        PageStep::More { next_after: 2 }
    );
    assert!(!state.history_complete);
    // The todo_write intent was on page one, its list is on page two.
    assert_eq!(
        step(&mut mapper, &mut state, &second, 2),
        PageStep::Complete
    );
    assert!(
        matches!(&state.transcript.items[..], [Item::User { .. }, Item::Plan(p), Item::Answer { .. }] if p.len() == 1)
    );
    // A cursor that does not move is not a history that is whole.
    let stuck = page(vec![], 2, true);
    assert_eq!(step(&mut mapper, &mut state, &stuck, 2), PageStep::Stalled);
    let behind = page(vec![], 0, true);
    assert_eq!(step(&mut mapper, &mut state, &behind, 2), PageStep::Stalled);
}

#[test]
fn an_old_entry_is_ignored_by_the_model_and_the_mapper_adds_no_state_to_it() {
    let mut t = Task::new();
    t.entries(&[
        user(5, "go"),
        user(4, "old"),
        user(5, "again"),
        answer(6, "ok"),
    ]);
    assert_eq!(t.shape(), "U A");
}

#[test]
fn a_new_session_with_no_history_is_complete_at_once() {
    let mut t = Task::new();
    assert!(!t.state.status().complete);
    t.state.history_complete = true;
    assert!(t.state.status().complete && !t.state.status().started);
}
