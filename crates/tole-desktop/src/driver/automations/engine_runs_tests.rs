//! The history of the selected automation: the selection, the calls and what an
//! answer, a late answer and a failure do.

use tole_desktop_model::{Load, RunOutcome};

use super::engine::{Input, RunsReply, STALE};
use super::fixtures::*;

#[test]
fn opening_an_automation_asks_for_its_history_and_the_answer_fills_it() {
    let mut t = T::known();
    let out = t.step(Input::Select(Some("on".into())));
    let (call, job) = runs_call_of(&out).expect("its history");
    assert_eq!(job, id("on"));
    let v = t.view();
    assert_eq!(v.history.load, Load::Loading);
    assert!(v.page.history.is_empty() && v.history.feed.loading);

    t.step(Input::Runs(
        call,
        runs_reply("on", runs_of("on", &["ok", "error", "awaiting_approval"])),
    ));
    let v = t.view();
    assert_eq!(
        (v.history.load.clone(), v.page.history.len()),
        (Load::Ready, 3)
    );
    assert!(v.history.links_known && v.history.more_at_least == 0);
    assert!(!v.history.feed.loading);
    assert_eq!(v.page.history[1].outcome, RunOutcome::Failed);
}

#[test]
fn an_id_that_is_not_a_row_selects_nothing() {
    let mut t = T::known();
    let out = t.step(Input::Select(Some("nobody".into())));
    assert!(!out.changed && runs_call_of(&out).is_none());
    assert!(t.page().selected.is_none());
    assert_eq!(t.view().history.load, Load::Idle);
}

#[test]
fn nothing_is_read_while_nothing_is_open() {
    let mut t = T::known();
    t.step(Input::Refresh);
    for _ in 0..80 {
        assert!(runs_call_of(&t.after(1_000)).is_none());
    }
}

#[test]
fn the_open_history_is_read_again_on_the_timer_and_on_request() {
    let mut t = T::known();
    t.open("on", &["ok"]);
    let (call, waited) = t.next_runs(60_000).expect("a refresh");
    assert_eq!(waited, 30_000);
    assert_eq!(
        t.view().history.load,
        Load::Ready,
        "the entries stay while it runs"
    );
    t.step(Input::Runs(
        call,
        runs_reply("on", runs_of("on", &["ok", "ok"])),
    ));
    assert_eq!(t.page().history.len(), 2);
    let out = t.step(Input::Select(Some("on".into())));
    assert!(!out.changed, "opening the open one clears nothing");
    assert!(
        runs_call_of(&t.after(1_000)).is_some(),
        "it asks for a fresh read"
    );
    assert_eq!(t.page().history.len(), 2);
}

#[test]
fn an_answer_for_the_automation_that_was_open_before_is_not_shown() {
    let mut t = T::known();
    let first = runs_call_of(&t.step(Input::Select(Some("on".into()))))
        .expect("a call")
        .0;
    let out = t.step(Input::Select(Some("off".into())));
    assert!(
        runs_call_of(&out).is_none(),
        "the slot is held by the first call"
    );
    assert!(t.page().history.is_empty());

    let late = Input::Runs(first, runs_reply("on", runs_of("on", &["ok"])));
    t.step(late);
    assert!(
        t.page().history.is_empty(),
        "never another automation's runs"
    );
    let (second, job) = runs_call_of(&t.after(1_000)).expect("the history of the new one");
    assert_eq!(job, id("off"));
    t.step(Input::Runs(
        second,
        runs_reply("off", runs_of("off", &["error"])),
    ));
    let v = t.view();
    assert_eq!(
        (v.page.selected, v.page.history.len()),
        (Some(id("off")), 1)
    );
    assert_eq!(v.page.history[0].outcome, RunOutcome::Failed);
}

/// Open, switch away and come back: the call that was out for the first time is
/// about the same automation, but it began before the history was closed, and
/// it must not fill the new one.
#[test]
fn an_answer_for_the_same_automation_from_before_it_was_reopened_is_not_shown() {
    let mut t = T::known();
    let (first, _) = runs_call_of(&t.step(Input::Select(Some("on".into())))).expect("a call");
    t.step(Input::Select(Some("off".into())));
    t.step(Input::Select(Some("on".into())));
    t.step(Input::Runs(
        first,
        runs_reply("on", runs_of("on", &["ok", "ok"])),
    ));
    let v = t.view();
    assert!(v.page.history.is_empty(), "the old read is not this one");
    assert_eq!(
        (v.history.load, v.history.feed.failures),
        (Load::Loading, 0)
    );
    let (second, job) = runs_call_of(&t.after(1_000)).expect("a read that began after");
    assert_eq!(job, id("on"));
    t.step(Input::Runs(
        second,
        runs_reply("on", runs_of("on", &["error"])),
    ));
    assert_eq!(t.page().history.len(), 1);
}

#[test]
fn an_answer_that_names_another_automation_than_the_selected_one_is_dropped() {
    let mut t = T::known();
    let (call, _) = runs_call_of(&t.step(Input::Select(Some("on".into())))).expect("a call");
    let wrong = RunsReply {
        job: id("off"),
        runs: Ok(runs_of("off", &["ok"])),
        lookup: None,
    };
    t.step(Input::Runs(call, wrong));
    assert!(t.page().history.is_empty());
    assert_eq!(t.view().history.load, Load::Loading);
}

#[test]
fn closing_the_history_forgets_it_and_ignores_the_call_that_was_out() {
    let mut t = T::known();
    let (call, _) = runs_call_of(&t.step(Input::Select(Some("on".into())))).expect("a call");
    assert!(t.step(Input::Select(None)).changed);
    let v = t.view();
    assert!(v.page.selected.is_none() && v.history.load == Load::Idle);
    let out = t.step(Input::Runs(call, runs_reply("on", runs_of("on", &["ok"]))));
    assert!(t.page().history.is_empty() && runs_call_of(&out).is_none());
    assert!(!t.step(Input::Select(None)).changed, "already closed");
}

#[test]
fn a_failed_history_keeps_its_entries_marked_and_is_retried_with_a_growing_wait() {
    let fail = || RunsReply {
        job: id("on"),
        runs: Err(io_error()),
        lookup: None,
    };
    let mut t = T::known();
    t.open("on", &["ok"]);
    let (mut call, waited) = t.next_runs(60_000).expect("a refresh");
    assert_eq!(waited, 30_000);
    let mut waits = Vec::new();
    for _ in 0..4 {
        t.step(Input::Runs(call, fail()));
        let v = t.view();
        assert!(matches!(v.history.load, Load::Failed(_)));
        assert_eq!(v.page.history.len(), 1, "the last entries stay, marked");
        let (next, waited) = t.next_runs(60_000).expect("a retry");
        call = next;
        waits.push(waited / 1_000);
    }
    assert_eq!(waits, [1, 2, 4, 8]);
    t.step(Input::Runs(
        call,
        runs_reply("on", runs_of("on", &["ok", "ok"])),
    ));
    let v = t.view();
    assert_eq!(v.history.load, Load::Ready);
    assert_eq!((v.history.feed.failures, v.page.history.len()), (0, 2));
}

#[test]
fn an_old_history_is_marked_and_a_suspend_marks_it_too() {
    let mut t = T::known();
    t.open("on", &["ok"]);
    t.after(1_000);
    t.jump(3_600_000);
    let v = t.view();
    assert_eq!(v.history.load, Load::Failed(STALE.into()));
    assert_eq!(v.page.history.len(), 1, "the entries stay");
}

#[test]
fn a_run_that_stopped_to_ask_says_when_its_link_could_not_be_looked_up() {
    let mut t = T::known();
    let (call, _) = runs_call_of(&t.step(Input::Select(Some("on".into())))).expect("a call");
    let runs = runs_answer(vec![run_json(
        "f",
        "on",
        Some("awaiting_approval"),
        Some("s1"),
    )]);
    let reply = RunsReply {
        job: id("on"),
        runs: Ok(runs),
        lookup: Some(Err(io_error())),
    };
    t.step(Input::Runs(call, reply));
    let v = t.view();
    assert_eq!(v.history.load, Load::Ready);
    assert!(!v.history.links_known);
    assert_eq!(v.page.history[0].approval_link(), None);
}

#[test]
fn a_run_that_stopped_to_ask_links_to_its_pending_approval() {
    let mut t = T::known();
    let (call, _) = runs_call_of(&t.step(Input::Select(Some("on".into())))).expect("a call");
    let runs = runs_answer(vec![run_json(
        "f",
        "on",
        Some("awaiting_approval"),
        Some("s1"),
    )]);
    let pending = approvals_answer(vec![approval_json("apr-1", "s1", "pending")]);
    let reply = RunsReply {
        job: id("on"),
        runs: Ok(runs),
        lookup: Some(Ok(pending)),
    };
    t.step(Input::Runs(call, reply));
    let v = t.view();
    assert!(v.history.links_known);
    assert_eq!(v.page.history[0].approval_link(), Some("apr-1"));
}
