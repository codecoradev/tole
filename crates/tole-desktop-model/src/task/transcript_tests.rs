use super::event::{PlanItem, PlanStatus, TaskEvent as E};
use super::transcript::{Item, StepState, Transcript};

#[rustfmt::skip]
fn user(seq: u64) -> E { E::UserMessage { seq, text: "go".into(), images: 0 } }

#[rustfmt::skip]
fn answer(seq: u64, text: &str) -> E { E::AnswerEntry { seq, text: text.into() } }

fn delta(text: &str) -> E {
    E::TextDelta { text: text.into() }
}

/// An intent at `seq`, settled at `seq + 1` when `ok` is given.
#[rustfmt::skip]
fn call(seq: u64, tool: &str, ok: Option<bool>) -> Vec<E> {
    let id = format!("e{seq}");
    let intent = E::ToolIntent { seq, id: id.clone(), tool: tool.into(), target: None };
    let settled = ok.map(|ok| E::ToolSettled { seq: seq + 1, intent_id: id, ok });
    std::iter::once(intent).chain(settled).collect()
}

#[rustfmt::skip]
fn plan(status: PlanStatus) -> E {
    E::PlanUpdated { items: vec![PlanItem { text: "one".into(), status }] }
}

fn run(parts: Vec<Vec<E>>) -> Transcript {
    let mut t = Transcript::default();
    parts.concat().iter().for_each(|e| t.apply(e));
    t
}

/// The rows in short: U user, A answer, a draft, W3 a group of 3 steps, P plan.
fn shape(t: &Transcript) -> String {
    let row = |i: &Item| match i {
        Item::User { .. } => "U".to_string(),
        Item::Answer {
            streaming: true, ..
        } => "a".to_string(),
        Item::Answer { .. } => "A".to_string(),
        Item::Work(g) => format!("W{}", g.steps.len()),
        Item::Plan(_) => "P".to_string(),
    };
    t.items.iter().map(row).collect::<Vec<_>>().join(" ")
}

#[test]
fn calls_with_nothing_between_are_one_group_and_a_message_splits_it() {
    let two = vec![
        vec![user(1)],
        call(2, "read_file", Some(true)),
        call(4, "read_file", Some(true)),
    ];
    let mut t = run(two);
    assert_eq!(
        (shape(&t), t.step_count(), t.counts.read),
        ("U W2".into(), 2, 2)
    );
    [0, 9, 1].iter().for_each(|i| t.toggle_group(*i));
    assert!(matches!(&t.items[1], Item::Work(g) if g.expanded));
    let split = run(vec![
        call(2, "read_file", None),
        vec![answer(3, "x")],
        call(4, "read_file", None),
    ]);
    assert_eq!(shape(&split), "W1 A W1");
    assert_eq!(
        shape(&run(vec![vec![user(5), user(5), user(4), user(6)]])),
        "U U"
    );
}

#[test]
fn only_a_successful_change_counts_as_changed() {
    let t = run(vec![
        call(1, "write_file", Some(false)),
        call(3, "edit_file", Some(true)),
    ]);
    let Item::Work(g) = &t.items[0] else { panic!() };
    assert_eq!(t.counts.changed, 1);
    assert_eq!(
        (g.steps[0].state, g.steps[1].state),
        (StepState::Failed, StepState::Done)
    );
}

#[test]
fn an_old_entry_is_ignored() {
    assert_eq!(
        shape(&run(vec![vec![user(5), user(5), user(4), user(6)]])),
        "U U"
    );
}

#[test]
fn the_final_entry_replaces_the_draft_and_a_tool_call_keeps_it_as_a_note() {
    let t = run(vec![vec![delta("Hel"), delta("lo")]]);
    assert!(
        matches!(&t.items[..], [Item::Answer { markdown, streaming: true }] if markdown == "Hello")
    );
    let done = run(vec![vec![delta("Hel"), answer(9, "Hello.")]]);
    assert!(
        matches!(&done.items[..], [Item::Answer { markdown, streaming: false }] if markdown == "Hello.")
    );
    assert_eq!(
        shape(&run(vec![
            vec![delta("Looking.")],
            call(1, "read_file", None)
        ])),
        "A W1"
    );
}

#[test]
fn plans_are_not_steps_and_a_new_plan_replaces_the_old_one_of_the_turn() {
    let first = vec![
        vec![user(1)],
        call(2, "update_plan", None),
        vec![plan(PlanStatus::Pending)],
    ];
    let mut t = run(first);
    let more = [
        call(3, "read_file", None),
        vec![plan(PlanStatus::Completed)],
    ]
    .concat();
    more.iter()
        .chain(&[user(4), plan(PlanStatus::Pending)])
        .for_each(|e| t.apply(e));
    assert_eq!(shape(&t), "U P W1 U P");
}
