//! The answer is model text: made printable and capped like everything else.

use super::event::MAX_ANSWER_CHARS;
use super::*;

#[rustfmt::skip]
fn answer(seq: u64, text: &str) -> TaskEvent { TaskEvent::AnswerEntry { seq, text: text.into() } }

#[rustfmt::skip]
fn delta(text: &str) -> TaskEvent { TaskEvent::TextDelta { text: text.into() } }

/// The answer rows as (text, streaming, cut).
fn answers(t: &TaskState) -> Vec<(String, bool, bool)> {
    let rows = t.transcript.items.iter();
    rows.filter_map(|i| match i {
        Item::Answer {
            markdown,
            streaming,
            cut,
        } => Some((markdown.clone(), *streaming, *cut)),
        _ => None,
    })
    .collect()
}

#[test]
fn an_answer_keeps_its_lines_and_loses_what_hides_or_reorders_text() {
    let mut t = TaskState::new(false);
    t.apply(answer(
        1,
        "# T\r\nli\u{202E}ne\u{200B}2\u{1b}[0m\tend\u{2028}x\ry\u{FEFF}",
    ));
    assert_eq!(
        answers(&t),
        [("# T\nline2 [0m\tend\nxy".to_string(), false, false)]
    );
}

#[test]
fn a_huge_answer_is_cut_to_the_cap_and_says_so() {
    let mut t = TaskState::new(false);
    t.apply(answer(1, &"a".repeat(1_000_000)));
    let [(text, _, cut)] = &answers(&t)[..] else {
        panic!()
    };
    assert_eq!(text.chars().count(), MAX_ANSWER_CHARS);
    assert!(*cut && text.ends_with('\u{2026}'));
}

#[test]
fn endless_deltas_never_pass_the_cap_or_split_a_character() {
    let mut t = TaskState::new(false);
    let chunk = format!("{}\u{202E}", "é€".repeat(500));
    for _ in 0..10_000 {
        t.apply(delta(&chunk));
    }
    let [(text, streaming, cut)] = &answers(&t)[..] else {
        panic!()
    };
    assert!(text.chars().count() <= MAX_ANSWER_CHARS && text.ends_with('\u{2026}'));
    assert!(*streaming && *cut && !text.contains('\u{202E}'));
    t.apply(answer(9, "Done."));
    assert_eq!(answers(&t), [("Done.".to_string(), false, false)]);
}

#[test]
fn a_delta_stream_and_its_final_entry_are_one_clean_row() {
    let mut t = TaskState::new(false);
    t.apply(delta("Hel\u{202E}"));
    t.apply(delta("lo\r\n"));
    assert_eq!(answers(&t), [("Hello\n".to_string(), true, false)]);
    t.apply(TaskEvent::TurnEnded);
    t.apply(answer(5, "Hello."));
    assert_eq!(answers(&t), [("Hello.".to_string(), false, false)]);
}
