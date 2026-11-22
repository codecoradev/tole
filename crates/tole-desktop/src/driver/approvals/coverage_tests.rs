//! When the list may be called known (the followed streams), and the engine's
//! deadlines against a wall clock that steps.

use std::io;

use tole_desktop_model::Badge;

use super::engine::Input;
use super::engine_tests::{fetch_of, io_err, list, row, T};

fn pending() -> Vec<serde_json::Value> {
    vec![row("a1", "s1", "pending")]
}

#[test]
fn a_fetch_that_began_before_the_streams_were_up_does_not_make_the_list_known() {
    let mut t = T::with(&["s1", "s2"], super::engine_tests::REFRESH);
    let first = fetch_of(&t.start()).expect("first fetch");
    t.step(Input::StreamUp("s1".into()));
    // One stream is still down: even a fetch that begins now is not enough.
    t.step(Input::Fetched(first, Ok(list(pending()))));
    assert_eq!(t.view().badge, Badge::Unknown);
    t.step(Input::StreamUp("s2".into()));
    let second = t
        .fetch_within(2_000)
        .expect("every stream is up: fetch again");
    t.step(Input::Fetched(second, Ok(list(pending()))));
    assert_eq!(t.view().badge, Badge::Count(1));
}

#[test]
fn a_stream_that_drops_keeps_the_view_unknown_however_fast_the_fetch_is() {
    let mut t = T::known_following(pending(), &["s1", "s2"]);
    t.step(Input::StreamDown("s2".into()));
    assert_eq!(t.view().badge, Badge::Unknown);
    // The serve answers at once; the stream is still down.
    let id = t.fetch_within(2_000).expect("a fetch follows the loss");
    t.step(Input::Fetched(id, Ok(list(pending()))));
    assert_eq!(t.view().badge, Badge::Unknown, "the stream of s2 is down");
    assert_eq!(
        t.composer("s9"),
        tole_desktop_model::approvals::ComposerState::Checking
    );
    // It comes up; a fetch that starts after it makes the list known.
    t.step(Input::StreamUp("s2".into()));
    let id = t
        .fetch_within(2_000)
        .expect("a fetch after the stream is up");
    t.step(Input::Fetched(id, Ok(list(pending()))));
    assert_eq!(t.view().badge, Badge::Count(1));
}

#[test]
fn a_stream_that_drops_during_a_fetch_discards_what_that_fetch_proves() {
    let mut t = T::known_following(pending(), &["s1"]);
    let id = fetch_of(&t.after(15_000)).expect("refresh");
    t.step(Input::StreamDown("s1".into()));
    t.step(Input::StreamUp("s1".into()));
    t.step(Input::Fetched(id, Ok(list(pending()))));
    assert_eq!(
        t.view().badge,
        Badge::Unknown,
        "it began before the stream came back"
    );
    let id = t.fetch_within(2_000).expect("fetch again");
    t.step(Input::Fetched(id, Ok(list(pending()))));
    assert_eq!(t.view().badge, Badge::Count(1));
}

#[test]
fn a_parked_session_stops_holding_the_list_unknown_and_rejoins() {
    let mut t = T::known_following(pending(), &["s1", "gone"]);
    t.step(Input::StreamDown("gone".into()));
    let id = t.fetch_within(2_000).expect("fetch");
    t.step(Input::Fetched(id, Ok(list(pending()))));
    assert_eq!(
        t.view().badge,
        Badge::Unknown,
        "waiting for a stream that never comes"
    );
    // Refused again and again: parked, the list is known without it, and the
    // view says which session is only covered by the refresh.
    t.step(Input::StreamParked("gone".into()));
    let id = t.fetch_within(2_000).expect("fetch after the parking");
    t.step(Input::Fetched(id, Ok(list(pending()))));
    assert_eq!(t.view().badge, Badge::Count(1));
    assert_eq!(t.view().parked_sessions, vec!["gone".to_owned()]);
    // Its stream opens again (the task was resumed): it rejoins the coverage
    // and the next fetch is the one that counts.
    t.step(Input::StreamUp("gone".into()));
    assert!(t.view().parked_sessions.is_empty());
    let id = t.fetch_within(2_000).expect("a fetch after the rejoin");
    t.step(Input::Fetched(id, Ok(list(pending()))));
    assert_eq!(t.view().badge, Badge::Count(1));
}

#[test]
fn a_parked_session_that_drops_again_holds_the_list_unknown_again() {
    let mut t = T::known_following(pending(), &["s1"]);
    t.step(Input::StreamParked("s1".into()));
    t.step(Input::StreamUp("s1".into()));
    t.step(Input::StreamDown("s1".into()));
    assert_eq!(t.view().badge, Badge::Unknown);
    assert!(t.view().parked_sessions.is_empty());
}

#[test]
fn a_wall_clock_that_steps_back_stalls_neither_a_retry_nor_a_refresh() {
    // Back 10 minutes after a failed fetch: the retry still comes after 1 s.
    let mut t = T::new();
    let id = fetch_of(&t.start()).expect("fetch");
    t.step(Input::Fetched(
        id,
        Err(io_err(io::ErrorKind::ConnectionReset)),
    ));
    t.now.wall -= 600_000;
    assert!(t.after(999).effects.is_empty());
    let id = fetch_of(&t.after(1)).expect("the retry, on the monotonic clock");
    // Back again after a success: the refresh still comes after 15 s.
    t.step(Input::Fetched(id, Ok(list(vec![]))));
    t.now.wall -= 600_000;
    assert!(t.after(14_999).effects.is_empty());
    assert!(fetch_of(&t.after(1)).is_some(), "the refresh");
}

#[test]
fn a_wall_clock_that_jumps_ahead_of_the_monotonic_one_is_a_loss_not_a_rushed_retry() {
    // A failed fetch: a day forward makes no retry come sooner.
    let mut t = T::new();
    let id = fetch_of(&t.start()).expect("fetch");
    t.step(Input::Fetched(
        id,
        Err(io_err(io::ErrorKind::ConnectionReset)),
    ));
    t.now.wall += 86_400_000;
    assert!(
        t.after(0).effects.is_empty(),
        "a day forward is no reason to retry now"
    );
    let id = fetch_of(&t.after(1_000)).expect("the retry, on the monotonic clock");
    // A known list: the machine sleeps 2 hours (the monotonic clock stops, the
    // wall clock does not): it is not known any more, and is fetched again.
    t.step(Input::Fetched(id, Ok(list(pending()))));
    assert_eq!(t.view().badge, Badge::Count(1));
    t.now.wall += 7_200_000;
    t.now.mono += 1_000;
    let out = t.step(Input::Tick);
    assert_eq!(t.view().badge, Badge::Unknown, "a suspend is a loss");
    // No backoff step: nothing failed, and the pace allows it. It is asked at
    // once.
    let id = fetch_of(&out).expect("fetched again");
    t.step(Input::Fetched(id, Ok(list(pending()))));
    assert_eq!(t.view().badge, Badge::Count(1));
}

#[test]
fn small_differences_between_the_clocks_and_a_step_back_are_no_loss() {
    let mut t = T::known(pending());
    // Under the gap (a slow tick, a small correction) and backwards: nothing.
    t.now.wall += 4_000;
    t.now.mono += 100;
    t.step(Input::Tick);
    assert_eq!(t.view().badge, Badge::Count(1));
    t.now.wall -= 3_600_000;
    t.now.mono += 100;
    t.step(Input::Tick);
    assert_eq!(t.view().badge, Badge::Count(1));
}
