//! The engine's rules, one input at a time, with no thread and no clock: what
//! it asks for after each loss, how it paces a failing fetch, and what it needs
//! before the list counts as known.

use std::io;
use std::sync::Arc;
use std::time::Duration;

use serde_json::{json, Value};
use tole_client::{ApprovalList, DecisionReply, Error, Event, StreamItem};
use tole_desktop_model::approvals::{ComposerState, DecisionIntent, FetchId, Outcome, Tab};
use tole_desktop_model::Badge;

use super::engine::{Effect, Engine, Input, Now, Out};
use super::store::Store;
use super::view::ApprovalsView;

pub(super) const T0: u64 = 1_700_000_000_500;
pub(super) const REFRESH: Duration = Duration::from_secs(15);

pub(super) fn row(id: &str, session: &str, status: &str) -> Value {
    json!({
        "id": id, "sessionId": session, "tool": "write_file",
        "description": "path: a.txt", "status": status,
        "createdAtMs": 1_700_000_000_000u64, "expiresInSeconds": 900,
        "summary": {"kind": "create_file", "target": "a.txt", "effect": "adds",
                    "insideWorkspace": true}
    })
}

pub(super) fn list(rows: Vec<Value>) -> ApprovalList {
    serde_json::from_value(json!({ "approvals": rows })).expect("a list")
}

pub(super) fn io_err(kind: io::ErrorKind) -> Error {
    Error::Io(io::Error::from(kind))
}

pub(super) fn status(code: u16) -> Error {
    Error::Status {
        status: code,
        message: "no".into(),
        body: Value::Null,
    }
}

pub(super) fn reply(id: &str, status: &str) -> DecisionReply {
    serde_json::from_value(json!({"id": id, "status": status})).expect("a reply")
}

/// An engine, its store and the time the test says it is.
pub(super) struct T {
    pub e: Engine,
    pub store: Arc<Store>,
    pub now: Now,
}

impl T {
    pub(super) fn with(sessions: &[&str], refresh: Duration) -> T {
        let store = Arc::new(Store::default());
        let sessions = sessions.iter().map(|s| (*s).to_owned()).collect();
        T {
            e: Engine::new(Arc::clone(&store), sessions, refresh),
            store,
            now: Now {
                wall: T0,
                mono: 1_000,
            },
        }
    }

    pub(super) fn new() -> T {
        T::with(&[], REFRESH)
    }

    pub(super) fn start(&mut self) -> Out {
        self.e.start(self.now)
    }

    pub(super) fn step(&mut self, input: Input) -> Out {
        self.e.step(input, self.now)
    }

    /// Both readings move by `ms`, then the engine hears a tick.
    pub(super) fn after(&mut self, ms: u64) -> Out {
        self.now.wall += ms;
        self.now.mono += ms;
        self.step(Input::Tick)
    }

    /// Move time in quarter seconds until a fetch is asked for, at most `ms`.
    pub(super) fn fetch_within(&mut self, ms: u64) -> Option<FetchId> {
        let mut spent = 0;
        while spent < ms {
            spent += 250;
            if let Some(id) = fetch_of(&self.after(250)) {
                return Some(id);
            }
        }
        None
    }

    pub(super) fn view(&self) -> ApprovalsView {
        self.store.view(self.now.wall)
    }

    pub(super) fn decided(&self, id: &str) -> Option<Outcome> {
        let rows = self.store.lock().rows(Tab::Decided, self.now.wall);
        rows.into_iter()
            .find(|r| r.id == id)
            .and_then(|r| r.outcome)
    }

    pub(super) fn waiting(&self, id: &str) -> bool {
        let rows = self.store.lock().rows(Tab::Waiting, self.now.wall);
        rows.iter().any(|r| r.id == id)
    }

    pub(super) fn composer(&self, task: &str) -> ComposerState {
        self.store.lock().composer(task)
    }

    /// Start, answer the first fetch with `rows`: a known list, no streams.
    pub(super) fn known(rows: Vec<Value>) -> T {
        let mut t = T::new();
        let id = fetch_of(&t.start()).expect("the first fetch");
        t.step(Input::Fetched(id, Ok(list(rows.clone()))));
        assert!(matches!(t.view().badge, Badge::Count(_)), "known");
        t
    }

    /// The same, with followed sessions whose streams come up while the first
    /// fetch runs: known after the second fetch.
    pub(super) fn known_following(rows: Vec<Value>, sessions: &[&str]) -> T {
        let mut t = T::with(sessions, REFRESH);
        let first = fetch_of(&t.start()).expect("the first fetch");
        for s in sessions {
            t.step(Input::StreamUp((*s).to_owned()));
        }
        t.step(Input::Fetched(first, Ok(list(rows.clone()))));
        assert_eq!(
            t.view().badge,
            Badge::Unknown,
            "that fetch began before the streams"
        );
        let second = t
            .fetch_within(2_000)
            .expect("a fetch after the streams are up");
        t.step(Input::Fetched(second, Ok(list(rows))));
        assert!(matches!(t.view().badge, Badge::Count(_)), "known");
        t
    }
}

/// The fetch the output asks for, if any.
pub(super) fn fetch_of(out: &Out) -> Option<FetchId> {
    out.effects.iter().find_map(|e| match e {
        Effect::Fetch(id) => Some(*id),
        Effect::Send(_) => None,
    })
}

pub(super) fn sent(out: &Out) -> Vec<&DecisionIntent> {
    out.effects
        .iter()
        .filter_map(|e| match e {
            Effect::Send(i) => Some(i),
            Effect::Fetch(_) => None,
        })
        .collect()
}

pub(super) fn stream(event: Event) -> Input {
    Input::Stream(StreamItem::Event(event))
}

#[test]
fn start_asks_for_the_whole_list_and_knows_nothing_until_it_arrives() {
    let mut t = T::new();
    let out = t.start();
    assert_eq!(out.effects, vec![Effect::Fetch(FetchId(1))]);
    assert_eq!(t.view().badge, Badge::Unknown);
    assert_eq!(t.composer("s1"), ComposerState::Checking);
    t.step(Input::Fetched(
        FetchId(1),
        Ok(list(vec![row("a1", "s1", "pending")])),
    ));
    let v = t.view();
    assert_eq!(v.badge, Badge::Count(1));
    assert_eq!(
        t.composer("s1"),
        ComposerState::WaitingForAnswer { requests: 1 }
    );
    assert_eq!(t.composer("s2"), ComposerState::Open);
    assert_eq!(
        v.composers,
        vec![("s1".into(), ComposerState::WaitingForAnswer { requests: 1 })]
    );
}

#[test]
fn every_form_of_loss_is_unknown_at_once_and_followed_by_a_fetch() {
    let undecodable = Event::Other {
        event: "approval_requested".into(),
        data: json!([1, 2, 3]),
    };
    let losses = vec![
        (
            "reconnected",
            Input::Stream(StreamItem::Reconnected { after: 0 }),
        ),
        ("resync", stream(Event::Resync { dropped: 3 })),
        ("closed", stream(Event::Closed { id: "s1".into() })),
        ("undecodable frame", stream(undecodable)),
        ("stream error or end", Input::StreamDown("s1".into())),
    ];
    for (name, loss) in losses {
        let mut t = T::known_following(vec![row("a1", "s1", "pending")], &["s1"]);
        let out = t.step(loss);
        assert!(
            out.effects.is_empty(),
            "{name}: the fetch follows after the pace"
        );
        assert_eq!(t.view().badge, Badge::Unknown, "{name}");
        assert_eq!(t.composer("s2"), ComposerState::Checking, "{name}");
        // A stream that is down is up again before the fetch can count.
        t.step(Input::StreamUp("s1".into()));
        assert!(t.fetch_within(3_000).is_some(), "{name}: a fetch follows");
    }
    // A heartbeat is not a loss.
    let mut t = T::known(vec![row("a1", "s1", "pending")]);
    let out = t.step(Input::Stream(StreamItem::Heartbeat));
    assert!(out.effects.is_empty() && !out.changed);
    assert_eq!(t.view().badge, Badge::Count(1));
}

#[test]
fn a_loss_while_a_fetch_runs_waits_its_turn_and_distrusts_the_old_reply() {
    let mut t = T::new();
    assert_eq!(fetch_of(&t.start()), Some(FetchId(1)));
    // Fetch 1 is out; the stream drops: another fetch is wanted, but only one
    // call is out at a time.
    t.step(stream(Event::Resync { dropped: 1 }));
    assert!(t.fetch_within(5_000).is_none());
    // The late reply of fetch 1 lists a request, and must not make the list known.
    let late = list(vec![row("a1", "s1", "pending")]);
    // (Its pace is long over by now, so the next fetch starts with this reply.)
    let out = t.step(Input::Fetched(FetchId(1), Ok(late)));
    let v = t.view();
    assert_eq!(
        v.badge,
        Badge::Unknown,
        "a superseded reply never makes it known"
    );
    assert_eq!(t.composer("s2"), ComposerState::Checking);
    let id = fetch_of(&out).expect("then the fetch that follows the loss");
    assert_eq!(id, FetchId(2));
    t.step(Input::Fetched(
        id,
        Ok(list(vec![row("a1", "s1", "pending")])),
    ));
    assert_eq!(t.view().badge, Badge::Count(1));
}

#[test]
fn a_failed_fetch_is_retried_with_a_growing_wait_and_never_given_up() {
    let mut t = T::new();
    let mut id = fetch_of(&t.start()).expect("fetch");
    for secs in [1u64, 2, 4, 8, 16, 30, 30, 30, 30] {
        let out = t.step(Input::Fetched(
            id,
            Err(io_err(io::ErrorKind::ConnectionReset)),
        ));
        assert!(out.effects.is_empty());
        assert_eq!(t.view().badge, Badge::Unknown);
        assert_eq!(t.composer("s1"), ComposerState::Checking);
        let early = t.after(secs * 1000 - 1);
        assert!(early.effects.is_empty(), "no retry before {secs} s");
        assert!(t.after(0).effects.is_empty());
        id = fetch_of(&t.after(1)).unwrap_or_else(|| panic!("the retry after {secs} s"));
    }
}

#[test]
fn only_a_success_resets_the_backoff_and_a_loss_does_not() {
    let mut t = T::new();
    let mut id = fetch_of(&t.start()).expect("fetch");
    // Three failures: the next wait would be 8 s.
    for secs in [1u64, 2, 4] {
        t.step(Input::Fetched(id, Err(io_err(io::ErrorKind::TimedOut))));
        id = fetch_of(&t.after(secs * 1000)).expect("retry");
    }
    // A loss while that fetch is out does not start the sequence over.
    t.step(Input::Stream(StreamItem::Reconnected { after: 0 }));
    t.step(Input::Fetched(id, Err(io_err(io::ErrorKind::TimedOut))));
    assert!(
        t.after(7_999).effects.is_empty(),
        "the wait is 8 s, not 1 s"
    );
    id = fetch_of(&t.after(1)).expect("retry after 8 s");
    // A success starts it over: a loss costs 1 s, and a failure after it 2 s,
    // not the 16 s the earlier failures would have made.
    t.step(Input::Fetched(id, Ok(list(vec![]))));
    assert_eq!(t.view().badge, Badge::Count(0));
    t.step(Input::Stream(StreamItem::Reconnected { after: 0 }));
    let id = t.fetch_within(2_000).expect("a fetch 1 s after the loss");
    // The loss counted as one step and this failure is the second: 2 s.
    t.step(Input::Fetched(id, Err(status(503))));
    assert!(t.after(1_999).effects.is_empty());
    assert!(fetch_of(&t.after(1)).is_some());
}

#[test]
fn losses_before_a_fetch_starts_are_one_loss_and_flapping_streams_do_not_hammer() {
    // Four streams drop together: one step of the backoff, not four.
    let sessions = ["s1", "s2", "s3", "s4"];
    let mut t = T::known_following(vec![], &sessions);
    for s in sessions {
        t.step(Input::StreamDown(s.into()));
        t.step(Input::StreamUp(s.into()));
    }
    let id = t.fetch_within(1_500).expect("one step of 1 s");
    t.step(Input::Fetched(id, Ok(list(vec![]))));
    assert_eq!(t.view().badge, Badge::Count(0));
    // A refusing server and four flapping streams, for a minute: the fetches
    // follow the backoff (1, 2, 4, ... 30 s), not the streams.
    let mut fetches = 0;
    let mut ticks = 0;
    let mut out = t.step(Input::StreamDown("s1".into()));
    while ticks < 62 * 4 {
        if let Some(id) = fetch_of(&out) {
            fetches += 1;
            t.step(Input::Fetched(
                id,
                Err(io_err(io::ErrorKind::ConnectionRefused)),
            ));
        }
        if ticks % 4 == 0 {
            for s in sessions {
                t.step(Input::StreamUp(s.into()));
                t.step(Input::StreamDown(s.into()));
            }
        }
        out = t.after(250);
        ticks += 1;
    }
    assert!(fetches <= 8, "{fetches} fetches in 62 s");
    assert!(fetches >= 4, "but it keeps trying: {fetches}");
}

#[test]
fn the_model_asking_for_a_refresh_never_bypasses_the_pace() {
    let mut t = T::known(vec![row("a1", "s1", "pending")]);
    // A 409 makes the list stale; the refresh is asked for, within the pace.
    let out = t.step(Input::Decide(
        "a1".into(),
        tole_desktop_model::approvals::Decision::AllowOnce,
    ));
    assert_eq!(sent(&out).len(), 1);
    let intent = sent(&out)[0].clone();
    let out = t.step(Input::Decided(intent, Err(status(409))));
    assert!(out.effects.is_empty(), "never more than one fetch a second");
    let id = t
        .fetch_within(1_500)
        .expect("a conflict makes the list stale: refetch");
    // That fetch fails: the list stays stale, but only the backoff decides.
    t.step(Input::Fetched(
        id,
        Err(io_err(io::ErrorKind::ConnectionReset)),
    ));
    for _ in 0..9 {
        assert!(t.after(100).effects.is_empty(), "one call per backoff step");
    }
    assert!(fetch_of(&t.after(100)).is_some());
}

#[test]
fn a_periodic_refresh_keeps_the_list_known_while_it_runs() {
    let mut t = T::new();
    let id = fetch_of(&t.start()).expect("fetch");
    t.step(Input::Fetched(id, Ok(list(vec![]))));
    assert!(t.after(14_999).effects.is_empty());
    let id = fetch_of(&t.after(1)).expect("refresh due");
    assert_eq!(t.view().badge, Badge::Count(0), "a refresh is not a loss");
    t.step(Input::Fetched(
        id,
        Ok(list(vec![row("a2", "s1", "pending")])),
    ));
    assert_eq!(t.view().badge, Badge::Count(1));
    assert!(t.after(14_999).effects.is_empty());
    assert!(fetch_of(&t.after(1)).is_some());
}

#[test]
fn a_list_older_than_twice_the_refresh_is_not_known_any_more() {
    let mut t = T::known(vec![row("a1", "s1", "pending")]);
    // The refresh starts at 15 s and hangs.
    assert!(fetch_of(&t.after(15_000)).is_some());
    assert!(matches!(t.after(14_000).effects.len(), 0));
    assert_eq!(
        t.view().badge,
        Badge::Count(1),
        "29 s old: still inside twice the refresh"
    );
    t.after(2_000);
    assert_eq!(t.view().badge, Badge::Unknown, "31 s old");
    assert_eq!(t.composer("s2"), ComposerState::Checking);
}

#[test]
fn a_poisoned_lock_throws_the_state_away_and_asks_again() {
    let mut t = T::known(vec![row("a1", "s1", "pending")]);
    let store = Arc::clone(&t.store);
    let crashed = std::thread::spawn(move || {
        let _guard = store.lock();
        panic!("a panic while the state is locked");
    })
    .join();
    assert!(crashed.is_err());
    t.after(10);
    assert_eq!(t.view().badge, Badge::Unknown, "fail closed, not stale");
    assert_eq!(t.composer("s1"), ComposerState::Checking);
    assert!(
        t.fetch_within(2_000).is_some(),
        "the lost state is fetched again"
    );
}

#[test]
fn the_worker_ticks_the_model_between_inputs_without_a_tick_input() {
    // Expiry runs only on a model tick; any input moves it when a second passed.
    let mut t = T::known(vec![row("a1", "s1", "pending")]);
    t.now.wall += 901_000;
    t.now.mono += 901_000;
    t.step(Input::Select(None));
    assert_eq!(t.decided("a1"), Some(Outcome::Expired));
}

#[test]
fn the_refresh_is_held_between_a_floor_and_a_ceiling() {
    use super::{effective_refresh, MAX_REFRESH, MIN_REFRESH};
    let e = |d| effective_refresh(d, MAX_REFRESH);
    assert_eq!(e(Duration::ZERO), MIN_REFRESH);
    assert_eq!(e(Duration::from_secs(15)), Duration::from_secs(15));
    assert_eq!(e(Duration::MAX), MAX_REFRESH);
    assert_eq!(e(Duration::from_secs(3600)), MAX_REFRESH);
}

#[test]
fn followed_sessions_have_no_duplicates_no_empty_ids_and_a_cap() {
    use super::{followed_sessions, MAX_STREAMS};
    let names = |v: Vec<&str>| v.into_iter().map(str::to_owned).collect::<Vec<_>>();
    assert_eq!(
        followed_sessions(names(vec!["a", "a", "", "b", "a"])),
        names(vec!["a", "b"])
    );
    let many = names(vec!["1", "2", "3", "4", "5", "6"]);
    assert_eq!(followed_sessions(many).len(), MAX_STREAMS);
}
