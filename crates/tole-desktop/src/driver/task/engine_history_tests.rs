//! The history: pages until the cursor reaches the end, then the stream opens.

use tole_desktop_model::task::{Phase, SendMode};

use super::calls::{Call, Lane, Reply};
use super::fixtures::{answer, page, status_code, user};
use super::sim::Sim;
use crate::adapter::task::MAX_HISTORY_PAGES;

fn ask_page(sim: &mut Sim) -> u64 {
    match sim.take(Lane::Read) {
        Call::Entries { after } => after,
        _ => panic!("not an entries call"),
    }
}

#[test]
fn pages_are_read_until_the_cursor_reaches_the_end_and_then_the_stream_opens_from_it() {
    let mut sim = Sim::new(false);
    let v = sim.view();
    assert!(v.loading && !v.history_complete);
    assert_eq!(v.status.send, SendMode::Disabled(Phase::Offline));
    assert_eq!(ask_page(&mut sim), 0);
    let first = page(vec![user(1, "a"), answer(2, "b")], 0, true, 4);
    sim.answer(Reply::Entries(0, Ok(first)));
    assert!(sim.opened.is_empty(), "the stream waits for the history");
    assert_eq!(ask_page(&mut sim), 2);
    let second = page(vec![user(3, "c"), answer(4, "d")], 2, false, 4);
    sim.answer(Reply::Entries(2, Ok(second)));
    let v = sim.view();
    assert!(v.history_complete && !v.loading);
    assert_eq!((v.total_rows, sim.opened.clone()), (4, vec![4]));
    assert_eq!(
        v.status.send,
        SendMode::Disabled(Phase::Offline),
        "nothing is sent until a stream can bring the answer"
    );
    assert!(sim.none_pending(), "the queue waits for the stream");
    sim.up(false);
    assert_eq!(sim.view().status.send, SendMode::Send);
}

#[test]
fn a_failed_page_is_asked_again_after_a_growing_wait_and_never_given_up() {
    let mut sim = Sim::new(false);
    for wait in [1_000, 2_000, 4_000, 8_000, 16_000, 30_000, 30_000, 30_000] {
        assert_eq!(ask_page(&mut sim), 0);
        sim.answer(Reply::Entries(0, Err(status_code(500))));
        sim.advance(wait - 1);
        assert!(!sim.has(Lane::Read), "not before {wait} ms");
        sim.advance(1);
        assert!(sim.has(Lane::Read), "again at {wait} ms");
    }
    assert!(sim.view().loading && !sim.fatal);
}

#[test]
fn a_page_that_cannot_be_trusted_leaves_the_history_incomplete_and_the_stream_opens() {
    let mut sim = Sim::new(false);
    ask_page(&mut sim);
    // Out of order: the adapter calls it stalled.
    let bad = page(vec![user(5, "x"), user(3, "y")], 0, true, 9);
    sim.answer(Reply::Entries(0, Ok(bad)));
    let v = sim.view();
    assert!(
        !v.history_complete && !v.loading,
        "never claims it is whole"
    );
    assert_eq!(sim.opened, vec![0], "the stream replays the rest");
}

#[test]
fn a_history_that_never_ends_stops_at_the_page_bound() {
    let mut sim = Sim::new(false);
    for n in 1..=MAX_HISTORY_PAGES as u64 {
        let after = ask_page(&mut sim);
        let more = page(vec![user(n, "x")], after, true, u64::MAX);
        sim.answer(Reply::Entries(after, Ok(more)));
    }
    assert!(!sim.has(Lane::Read));
    assert!(!sim.view().history_complete);
    assert_eq!(sim.opened, vec![MAX_HISTORY_PAGES as u64]);
}

#[test]
fn a_new_session_has_no_history_and_its_stream_opens_at_once() {
    let sim = Sim::new(true);
    let v = sim.view();
    assert!(v.history_complete && !v.loading);
    assert_eq!(sim.opened, vec![0]);
    assert!(!sim.has(Lane::Read));
}

#[test]
fn entries_arriving_on_the_stream_after_the_history_are_not_lost_or_doubled() {
    let mut sim = Sim::new(false);
    ask_page(&mut sim);
    sim.answer(Reply::Entries(0, Ok(page(vec![user(1, "a")], 0, false, 1))));
    sim.up(false);
    // The stream replays from the cursor: entry 1 is not sent again, 2 is new.
    sim.entry(&user(1, "a"));
    sim.entry(&answer(2, "b"));
    assert_eq!(sim.view().total_rows, 2);
}
