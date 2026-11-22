//! Data from a server that is broken or hostile never panics the driver and
//! never makes the model say more than it knows.

use std::io;

use serde_json::json;
use tole_client::{EntriesPage, Error, PromptReply};
use tole_desktop_model::task::{Item, Phase};

use super::calls::{Call, Lane, Reply};
use super::engine::Input;
use super::fixtures::*;
use super::sim::Sim;
use super::store::Viewport;
use crate::adapter::task::MAX_REFRESH_ROWS;

/// The model's `MAX_ANSWER_CHARS` (not exported).
const MAX_ANSWER_CHARS: usize = 200_000;

fn first_page(sim: &mut Sim, page: EntriesPage) {
    match sim.take(Lane::Read) {
        Call::Entries { after } => sim.answer(Reply::Entries(after, Ok(page))),
        _ => panic!("not an entries call"),
    }
}

#[test]
fn a_page_that_promises_more_and_holds_nothing_is_not_followed() {
    let mut sim = Sim::new(false);
    first_page(&mut sim, page(vec![], 0, true, 50));
    assert!(!sim.view().history_complete);
    assert_eq!(sim.opened, vec![0]);
    assert!(!sim.has(Lane::Read), "no loop on an empty promise");
}

#[test]
fn a_cursor_that_lies_does_not_send_the_stream_past_the_entries() {
    let mut sim = Sim::new(false);
    let lying: EntriesPage = serde_json::from_value(json!({
        "id": S, "entries": [user(1, "x")], "nextAfter": u64::MAX,
        "hasMore": false, "lastSeq": 1}))
    .expect("a page");
    first_page(&mut sim, lying);
    assert!(!sim.view().history_complete, "never claims it is whole");
    assert_eq!(
        sim.opened,
        vec![0],
        "from the cursor that was proven, not the lie"
    );
}

#[test]
fn a_huge_answer_is_cut_in_the_model_and_a_huge_queue_is_capped_at_the_mapper() {
    let mut sim = Sim::new(false);
    let big = "x".repeat(MAX_ANSWER_CHARS + 50_000);
    first_page(&mut sim, page(vec![answer(1, &big)], 0, false, 1));
    let v = sim.view();
    assert!(matches!(
        v.rows.as_slice(),
        [Item::Answer { cut: true, markdown, .. }] if markdown.chars().count() <= MAX_ANSWER_CHARS
    ));
    for n in 0..(MAX_REFRESH_ROWS + 100) {
        sim.stream(requested(&format!("a{n}")));
    }
    assert!(sim.view().approvals.len() <= MAX_REFRESH_ROWS);
    sim.stream(requested(&"i".repeat(300)));
    assert!(sim.view().approvals.iter().all(|a| a.id.len() <= 64));
}

#[test]
fn a_flood_of_stream_errors_changes_nothing_after_the_first() {
    let mut sim = Sim::live();
    sim.stream_error(Error::Io(io::Error::other("reset")));
    let before = sim.view();
    for _ in 0..10_000 {
        sim.stream_error(Error::Io(io::Error::other("reset")));
    }
    assert_eq!(sim.view(), before);
    assert!(sim.none_pending());
}

#[test]
fn a_reply_with_a_stop_reason_nobody_knows_ends_the_turn() {
    let mut sim = Sim::live();
    sim.prompt("tidy");
    sim.entry(&user(1, "tidy"));
    let odd: PromptReply = serde_json::from_value(json!({"stopReason": "💥"})).expect("a reply");
    sim.answer_prompt(Ok(odd));
    assert_eq!(sim.phase(), Phase::Idle);
}

#[test]
fn measured_heights_that_make_no_sense_change_nothing_and_a_huge_batch_is_bounded() {
    let mut sim = Sim::live();
    sim.entry(&user(1, "a"));
    let total = sim.store.lock().task.layout.total();
    let junk = vec![(0, f32::NAN), (0, -5.0), (0, f32::INFINITY), (999, 10.0)];
    sim.feed(Input::Heights(junk));
    assert_eq!(sim.store.lock().task.layout.total(), total);
    sim.feed(Input::Heights((0..100_000).map(|n| (n, 10.0)).collect()));
    sim.feed(Input::Toggle(usize::MAX));
    assert!(!sim.fatal);
}

#[test]
fn a_viewport_that_is_not_a_number_still_gives_a_view() {
    let mut sim = Sim::live();
    sim.entry(&user(1, "a"));
    for (scroll_top, height, overscan) in [
        (f32::NAN, f32::NAN, f32::NAN),
        (-1e30, 0.0, -5.0),
        (1e30, 1e30, 1e30),
    ] {
        sim.store.lock().viewport = Viewport {
            scroll_top,
            height,
            overscan,
        };
        let v = sim.view();
        assert!(v.window.start <= v.window.end && v.window.end <= v.total_rows);
        assert_eq!(v.rows.len(), v.window.end - v.window.start);
    }
}

#[test]
fn a_page_that_repeats_entries_below_the_cursor_is_not_trusted() {
    let mut sim = Sim::new(false);
    first_page(&mut sim, page(vec![user(1, "a")], 0, true, 3));
    // The cursor is 1; the serve answers with entry 1 again.
    match sim.take(Lane::Read) {
        Call::Entries { after } => {
            let again = page(vec![user(1, "a")], 0, true, 3);
            sim.answer(Reply::Entries(after, Ok(again)));
        }
        _ => panic!("not an entries call"),
    }
    let v = sim.view();
    assert_eq!((v.total_rows, v.history_complete), (1, false));
    assert_eq!(sim.opened, vec![1]);
}
