//! Changing the followed sessions while the driver runs, on the engine alone:
//! what a change does to knowledge, how readers start and retire, and the cap
//! on the serve's stream slots. The tests keep the books a worker would: which
//! readers are alive, and they check after every step that no more than
//! `MAX_STREAMS` are, and that no session has two.

use std::collections::{BTreeMap, BTreeSet};

use tole_desktop_model::approvals::ComposerState;
use tole_desktop_model::Badge;

use super::engine::{Input, Out, ReaderId, ReaderOp};
use super::engine_tests::{fetch_of, list, row, REFRESH, T};
use super::{followed_sessions, MAX_STREAMS};

fn pending() -> Vec<serde_json::Value> {
    vec![row("a1", "s1", "pending")]
}

fn names(v: &[&str]) -> Vec<String> {
    v.iter().map(|s| (*s).to_owned()).collect()
}

fn start(id: ReaderId, session: &str) -> ReaderOp {
    ReaderOp::Start(id, session.to_owned())
}

/// An engine and the books of its readers.
struct S {
    t: T,
    alive: BTreeMap<ReaderId, String>,
    retired: BTreeSet<ReaderId>,
}

impl S {
    fn start(sessions: &[&str]) -> (S, Out) {
        let mut s = S {
            t: T::with(sessions, REFRESH),
            alive: BTreeMap::new(),
            retired: BTreeSet::new(),
        };
        let out = s.t.start();
        s.track(&out);
        (s, out)
    }

    /// Following `sessions`, every stream up and the list known (two fetches:
    /// the first began before the streams were up).
    fn known(sessions: &[&str], rows: Vec<serde_json::Value>) -> S {
        let (mut s, out) = S::start(sessions);
        let first = fetch_of(&out).expect("the first fetch");
        for name in sessions {
            s.step(Input::StreamUp((*name).to_owned()));
        }
        s.step(Input::Fetched(first, Ok(list(rows.clone()))));
        let second = s.t.fetch_within(2_000).expect("a fetch after the streams");
        s.step(Input::Fetched(second, Ok(list(rows))));
        assert!(matches!(s.t.view().badge, Badge::Count(_)), "known");
        s
    }

    fn track(&mut self, out: &Out) {
        for op in &out.readers {
            match op {
                ReaderOp::Start(id, session) => {
                    assert!(self.alive.insert(*id, session.clone()).is_none());
                }
                ReaderOp::Retire(id) => {
                    assert!(self.alive.contains_key(id), "retires a live reader");
                    assert!(self.retired.insert(*id), "retires once");
                }
            }
        }
        assert!(self.alive.len() <= MAX_STREAMS, "the cap on stream slots");
        let sessions: BTreeSet<_> = self.alive.values().collect();
        assert_eq!(sessions.len(), self.alive.len(), "one reader per session");
    }

    fn step(&mut self, input: Input) -> Out {
        let out = self.t.step(input);
        self.track(&out);
        out
    }

    fn set(&mut self, sessions: &[&str]) -> Out {
        self.step(Input::SetSessions(names(sessions)))
    }

    /// The reader of `session` that has not been retired.
    fn id_of(&self, session: &str) -> ReaderId {
        let live = (self.alive.iter()).find(|(id, s)| *s == session && !self.retired.contains(id));
        *live.expect("a reader").0
    }

    /// The reader's thread ended.
    fn end(&mut self, id: ReaderId) -> Out {
        self.alive.remove(&id);
        self.retired.remove(&id);
        self.step(Input::ReaderEnded(id))
    }

    fn badge(&self) -> Badge {
        self.t.view().badge
    }

    fn parked(&self) -> Vec<String> {
        self.t.view().parked_sessions
    }
}

#[test]
fn the_starting_sessions_each_get_a_reader_and_the_list_waits_for_their_streams() {
    let (s, out) = S::start(&["s2", "s1"]);
    assert_eq!(out.readers, vec![start(1, "s1"), start(2, "s2")]);
    assert_eq!(s.badge(), Badge::Unknown);
    assert!(s.parked().is_empty(), "started, not waiting for a slot");
}

#[test]
fn a_session_added_to_a_known_list_makes_it_unknown_until_its_stream_and_a_fetch_follow() {
    let mut s = S::known(&["s1"], pending());
    let out = s.set(&["s1", "s2"]);
    assert_eq!(out.readers, vec![start(2, "s2")]);
    assert_eq!(s.badge(), Badge::Unknown, "the new set is not proved yet");
    assert_eq!(s.t.view().waiting_at_least, 1, "what was listed is kept");
    assert_eq!(s.t.composer("s9"), ComposerState::Checking);
    // The fetch it asks for begins before the new stream is up: not enough.
    let early = s.t.fetch_within(2_000).expect("a fetch");
    s.step(Input::Fetched(early, Ok(list(pending()))));
    assert_eq!(s.badge(), Badge::Unknown, "it began before the stream");
    s.step(Input::StreamUp("s2".into()));
    let late = s.t.fetch_within(2_000).expect("a fetch after the stream");
    s.step(Input::Fetched(late, Ok(list(pending()))));
    assert_eq!(s.badge(), Badge::Count(1));
}

#[test]
fn a_read_that_began_before_the_new_stream_was_up_never_makes_the_list_known() {
    // The read is out when the session is added, and answers after the stream
    // is up: it still began before.
    let mut s = S::known(&["s1"], pending());
    let out = s.t.after(15_000);
    let id = fetch_of(&out).expect("the refresh");
    s.set(&["s1", "s2"]);
    s.step(Input::StreamUp("s2".into()));
    s.step(Input::Fetched(id, Ok(list(pending()))));
    assert_eq!(s.badge(), Badge::Unknown);
    let next = s.t.fetch_within(2_000).expect("asked again");
    s.step(Input::Fetched(next, Ok(list(pending()))));
    assert_eq!(s.badge(), Badge::Count(1));
    // The same with the answer before the stream is up.
    let out = s.t.after(15_000);
    let id = fetch_of(&out).expect("the refresh");
    s.set(&["s1", "s2", "s3"]);
    s.step(Input::Fetched(id, Ok(list(pending()))));
    s.step(Input::StreamUp("s3".into()));
    assert_eq!(s.badge(), Badge::Unknown);
    let next = s.t.fetch_within(2_000).expect("asked again");
    s.step(Input::Fetched(next, Ok(list(pending()))));
    assert_eq!(s.badge(), Badge::Count(1));
}

#[test]
fn removing_a_session_loses_nothing_and_retires_its_reader() {
    let mut s = S::known(&["s1", "s2"], pending());
    let gone = s.id_of("s2");
    let out = s.set(&["s1"]);
    assert_eq!(out.readers, vec![ReaderOp::Retire(gone)]);
    assert!(out.effects.is_empty(), "no fetch for a smaller set");
    assert_eq!(s.badge(), Badge::Count(1));
    // The retired reader may still say something before it ends: not the
    // stream of any session that is followed.
    for late in [
        Input::StreamDown("s2".into()),
        Input::StreamUp("s2".into()),
        Input::StreamParked("s2".into()),
    ] {
        s.step(late);
        assert_eq!(s.badge(), Badge::Count(1));
        assert!(s.parked().is_empty());
    }
    s.end(gone);
    assert_eq!(s.badge(), Badge::Count(1));
}

#[test]
fn a_read_in_flight_when_a_session_is_removed_still_counts() {
    let mut s = S::known(&["s1", "s2"], pending());
    let out = s.t.after(15_000);
    let id = fetch_of(&out).expect("the refresh");
    s.set(&["s1"]);
    s.step(Input::Fetched(id, Ok(list(pending()))));
    assert_eq!(s.badge(), Badge::Count(1), "valid for the smaller set");
}

#[test]
fn removing_the_session_that_held_the_list_unknown_lets_a_fetch_make_it_known() {
    let mut s = S::known(&["s1", "s2"], pending());
    s.step(Input::StreamDown("s2".into()));
    assert_eq!(s.badge(), Badge::Unknown);
    // The fetch that follows the loss is no use while s2 is down.
    let early = s.t.fetch_within(2_000).expect("a fetch after the loss");
    s.step(Input::Fetched(early, Ok(list(pending()))));
    assert_eq!(s.badge(), Badge::Unknown);
    s.set(&["s1"]);
    let id =
        s.t.fetch_within(2_000)
            .expect("a fetch for the smaller set");
    s.step(Input::Fetched(id, Ok(list(pending()))));
    assert_eq!(s.badge(), Badge::Count(1));
}

#[test]
fn the_same_set_again_changes_nothing() {
    let mut s = S::known(&["s1", "s2"], pending());
    for same in [&["s1", "s2"][..], &["s2", "s1"], &["s2", "", "s1", "s2"]] {
        let out = s.set(same);
        assert!(out.readers.is_empty() && out.effects.is_empty());
        assert!(!out.changed, "nothing to show");
        assert_eq!(s.badge(), Badge::Count(1));
    }
}

#[test]
fn only_the_first_four_sessions_of_a_longer_set_are_followed() {
    let mut s = S::known(&["a"], pending());
    let out = s.set(&["a", "b", "c", "d", "e", "f"]);
    assert_eq!(
        out.readers,
        vec![start(2, "b"), start(3, "c"), start(4, "d")]
    );
    assert!(s.parked().is_empty());
}

#[test]
fn a_session_added_when_every_slot_is_taken_waits_for_one_and_the_list_stays_known() {
    let mut s = S::known(&["a", "b", "c", "d"], pending());
    let d = s.id_of("d");
    let out = s.set(&["a", "b", "c", "e"]);
    // d's reader holds its stream until it ends: e has no slot yet.
    assert_eq!(out.readers, vec![ReaderOp::Retire(d)]);
    assert_eq!(s.parked(), vec!["e".to_owned()], "covered by the refresh");
    assert_eq!(s.badge(), Badge::Count(1), "nothing was lost");
    // Nothing the clock or the other readers do frees a slot.
    s.t.after(5_000);
    s.step(Input::StreamDown("e".into()));
    assert_eq!(s.parked(), vec!["e".to_owned()]);
    // The old reader ends: e starts at once, and is not covered until up.
    let out = s.end(d);
    assert_eq!(out.readers, vec![start(5, "e")]);
    assert!(s.parked().is_empty());
    assert_eq!(s.badge(), Badge::Unknown);
    // The fetch it asks for at once began before the stream: not enough.
    let early = fetch_of(&out).expect("a fetch");
    s.step(Input::Fetched(early, Ok(list(pending()))));
    assert_eq!(s.badge(), Badge::Unknown);
    s.step(Input::StreamUp("e".into()));
    let id = s.t.fetch_within(2_000).expect("a fetch after the stream");
    s.step(Input::Fetched(id, Ok(list(pending()))));
    assert_eq!(s.badge(), Badge::Count(1));
}

#[test]
fn replacing_every_session_starts_each_new_one_as_an_old_reader_ends() {
    let mut s = S::known(&["a", "b", "c", "d"], pending());
    let old: Vec<ReaderId> = ["a", "b", "c", "d"].iter().map(|n| s.id_of(n)).collect();
    let out = s.set(&["w", "x", "y", "z"]);
    assert_eq!(out.readers.len(), 4, "four retirements, no start");
    assert!(out
        .readers
        .iter()
        .all(|op| matches!(op, ReaderOp::Retire(_))));
    assert_eq!(s.parked().len(), 4);
    for (n, id) in old.into_iter().enumerate() {
        let out = s.end(id);
        assert_eq!(out.readers.len(), 1, "one slot, one start");
        assert_eq!(s.parked().len(), 3 - n);
    }
    assert_eq!(s.badge(), Badge::Unknown, "the new streams are not up");
}

#[test]
fn a_session_removed_and_added_again_waits_for_its_old_reader_and_ignores_its_messages() {
    let mut s = S::known(&["s1"], pending());
    let old = s.id_of("s1");
    s.set(&[]);
    let out = s.set(&["s1"]);
    assert!(out.readers.is_empty(), "the old reader is still alive");
    assert_eq!(s.parked(), vec!["s1".to_owned()]);
    // What the old reader says is not about the stream to come.
    s.step(Input::StreamUp("s1".into()));
    s.step(Input::StreamDown("s1".into()));
    s.step(Input::StreamParked("s1".into()));
    assert_eq!(s.parked(), vec!["s1".to_owned()]);
    assert_eq!(s.badge(), Badge::Count(1));
    let out = s.end(old);
    assert_eq!(out.readers, vec![start(2, "s1")]);
    assert_eq!(s.badge(), Badge::Unknown);
    s.step(Input::StreamUp("s1".into()));
    let id = s.t.fetch_within(2_000).expect("a fetch after the stream");
    s.step(Input::Fetched(id, Ok(list(pending()))));
    assert_eq!(s.badge(), Badge::Count(1));
}

#[test]
fn a_reader_that_ends_on_its_own_is_replaced_and_an_unknown_one_is_ignored() {
    let mut s = S::known(&["s1"], pending());
    let out = s.step(Input::ReaderEnded(999));
    assert!(out.readers.is_empty() && !out.changed);
    let id = s.id_of("s1");
    let out = s.end(id);
    assert_eq!(out.readers, vec![start(2, "s1")]);
    assert_eq!(s.badge(), Badge::Unknown, "a session without a stream");
    s.step(Input::StreamUp("s1".into()));
    let id = s.t.fetch_within(2_000).expect("a fetch after the stream");
    s.step(Input::Fetched(id, Ok(list(pending()))));
    assert_eq!(s.badge(), Badge::Count(1));
}

#[test]
fn a_suspend_after_adding_a_session_keeps_the_list_unknown_until_a_fetch_that_counts() {
    let mut s = S::known(&["s1"], pending());
    s.set(&["s1", "s2"]);
    s.step(Input::StreamUp("s2".into()));
    let id = s.t.fetch_within(2_000).expect("a fetch after the stream");
    // The machine sleeps two hours while that fetch is out.
    s.t.now.wall += 7_200_000;
    s.t.now.mono += 1_000;
    s.step(Input::Tick);
    let out = s.step(Input::Fetched(id, Ok(list(pending()))));
    assert_eq!(s.badge(), Badge::Unknown, "a suspend is a loss");
    let id = fetch_of(&out)
        .or_else(|| s.t.fetch_within(2_000))
        .expect("fetched again");
    s.step(Input::Fetched(id, Ok(list(pending()))));
    assert_eq!(s.badge(), Badge::Count(1));
}

#[test]
fn ids_the_serve_cannot_have_given_are_not_followed() {
    let long = "x".repeat(1 << 20);
    let hostile = vec![
        long,
        "a\nb".to_owned(),
        "a\u{0}".to_owned(),
        "../etc".to_owned(),
        "a/b".to_owned(),
        "a b".to_owned(),
        "caf\u{e9}".to_owned(),
        "\u{202e}rtl".to_owned(),
        "a".repeat(65),
        String::new(),
        "ok_1-A".to_owned(),
        "ok_1-A".to_owned(),
        "a".repeat(64),
    ];
    assert_eq!(
        followed_sessions(hostile.clone()),
        vec!["ok_1-A".to_owned(), "a".repeat(64)]
    );
    let mut s = S::known(&["s1"], pending());
    let out = s.step(Input::SetSessions(hostile));
    assert_eq!(
        out.readers,
        vec![
            ReaderOp::Retire(1),
            start(2, "a".repeat(64).as_str()),
            start(3, "ok_1-A")
        ]
    );
}

#[test]
fn a_flood_of_changes_never_passes_the_cap_nor_runs_two_readers_of_one_session() {
    let sets: [&[&str]; 4] = [
        &[],
        &["a", "b", "c", "d"],
        &["e", "f", "g", "h"],
        &["a", "e"],
    ];
    let mut s = S::known(&["a", "b"], pending());
    for i in 0..4_000usize {
        s.set(sets[i % sets.len()]);
        // Readers end slowly: one of the retired ones, now and then.
        if i % 3 == 0 {
            if let Some(id) = s.retired.iter().next().copied() {
                s.end(id);
            }
        }
    }
    // Let every retired reader end: what is followed is the last set.
    while let Some(id) = s.retired.iter().next().copied() {
        s.end(id);
    }
    let last = sets[3999 % sets.len()];
    let followed: BTreeSet<&str> = s.alive.values().map(String::as_str).collect();
    assert_eq!(followed, last.iter().copied().collect::<BTreeSet<_>>());
}
