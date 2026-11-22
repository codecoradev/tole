//! The driver on its threads, over a scripted serve and a clock the test moves.

use std::io;
use std::time::Duration;

use tole_client::Error;
use tole_desktop_model::approvals::Decision;
use tole_desktop_model::task::{Item, Phase, SendMode};

use super::fixtures::*;
use super::rig::{fresh, Rig};
use super::{Config, LinkState};
use crate::adapter::task::MAX_STOP_POLLS;
use crate::driver::fake::FakeApi;

/// The pace of the watch after an allow or a Stop.
const POLL: Duration = Duration::from_millis(500);

fn answer_text(v: &super::TaskView) -> Option<String> {
    v.rows.iter().rev().find_map(|i| match i {
        Item::Answer { markdown, .. } => Some(markdown.clone()),
        _ => None,
    })
}

#[test]
fn the_history_is_replayed_then_the_stream_follows_a_live_turn() {
    let api = FakeApi::new();
    api.reply_entries(Ok(page(vec![user(1, "a"), answer(2, "b")], 0, true, 4)));
    api.reply_entries(Ok(page(vec![user(3, "c"), answer(4, "d")], 2, false, 4)));
    api.reply_list(Ok(list(vec![])));
    let stream = api.open_stream(S);
    let rig = Rig::start(api.clone(), Config::new(S));
    let v = rig.until("history", |v| v.history_complete);
    assert_eq!((v.total_rows, api.entries_calls()), (4, vec![0, 2]));
    api.wait_opens(S, 1);
    assert_eq!(
        api.afters(),
        vec![4],
        "the stream starts where the history ended"
    );
    Rig::up(&stream);
    api.wait_list_calls(1);
    assert_eq!(api.list_sessions(), vec![Some(S.to_string())]);
    rig.until("live", |v| v.link == LinkState::Live && !v.loading);

    // A live turn: the call is held, the stream carries the turn.
    let held = api.gate_prompt();
    rig.handle.send("hello").unwrap();
    api.wait_prompts(1);
    assert_eq!(api.prompts(), vec!["hello"]);
    rig.until("working", |v| v.status.phase == Phase::Working);
    Rig::feed(&stream, entry_item(&user(5, "hello")));
    Rig::feed(&stream, delta("Hi"));
    Rig::feed(&stream, entry_item(&answer(6, "Hi there")));
    let v = rig.until("answered", |v| {
        answer_text(v).as_deref() == Some("Hi there")
    });
    assert_eq!(v.status.phase, Phase::Idle);
    assert_eq!(
        v.status.send,
        SendMode::Disabled(Phase::Working),
        "the call is still out"
    );
    held.send(Ok(finished())).unwrap();
    rig.until("send open", |v| v.status.send == SendMode::Send);
}

#[test]
fn an_allow_is_polled_until_the_turn_is_confirmed_and_then_the_answer_ends_it() {
    let (api, rig, stream) = fresh();
    Rig::feed(&stream, requested("a1"));
    rig.until("the card", |v| v.status.phase == Phase::WaitingApproval);
    api.reply_decide(Ok(decided("a1", "approved")));
    rig.handle.decide("a1", Decision::AllowOnce).unwrap();
    api.wait_decides(1);
    assert_eq!(api.decides()[0].0, "a1");
    rig.until("resuming", |v| v.status.phase == Phase::Resuming);
    // Poll 1 says idle (serve answers 202 first): not the end. Poll 2 is held.
    api.reply_status(Ok(status(false)));
    let held = api.gate_status();
    rig.advance(POLL);
    api.wait_status(1);
    // The second poll is made only once the first answer was applied.
    rig.advance(POLL);
    api.wait_status(2);
    assert_eq!(rig.handle.snapshot().status.phase, Phase::Resuming);
    held.send(Ok(status(true))).unwrap();
    rig.until("confirmed", |v| v.status.phase == Phase::Working);
    Rig::feed(&stream, entry_item(&answer(3, "done")));
    rig.until("idle", |v| v.status.phase == Phase::Idle);
}

#[test]
fn a_409_is_polled_until_the_other_turn_is_over_and_the_words_come_back() {
    let (api, rig, _stream) = fresh();
    api.reply_prompt(Err(status_code(409)));
    let first = api.gate_status();
    api.reply_status(Ok(status(false)));
    rig.handle.send("hi").unwrap();
    api.wait_prompts(1);
    let v = rig.until("restored", |v| v.draft == "hi");
    assert_eq!(v.status.phase, Phase::Working, "someone else's turn runs");
    rig.advance(POLL);
    api.wait_status(1);
    first.send(Ok(status(true))).unwrap();
    rig.advance(POLL);
    api.wait_status(2);
    rig.until("idle", |v| v.status.phase == Phase::Idle);
}

#[test]
fn stop_answered_with_cancelled_false_ends_the_turn_at_once() {
    let (api, rig, stream) = fresh();
    let held = api.gate_prompt();
    api.reply_cancel(Ok(cancelled(false)));
    rig.handle.send("tidy").unwrap();
    api.wait_prompts(1);
    Rig::feed(&stream, entry_item(&user(3, "tidy")));
    rig.until("stoppable", |v| v.status.can_stop);
    rig.handle.stop().unwrap();
    api.wait_cancels(1);
    rig.until("idle", |v| v.status.phase == Phase::Idle);
    held.send(Ok(finished())).unwrap();
    rig.until("send open", |v| v.status.send == SendMode::Send);
}

#[test]
fn a_cancel_that_is_accepted_while_the_turn_runs_on_offers_stop_again_after_forty_polls() {
    let (api, rig, stream) = fresh();
    let held = api.gate_prompt();
    api.reply_cancel(Ok(cancelled(true)));
    api.always_status(status(true));
    rig.handle.send("tidy").unwrap();
    api.wait_prompts(1);
    Rig::feed(&stream, entry_item(&user(3, "tidy")));
    rig.until("stoppable", |v| v.status.can_stop);
    rig.handle.stop().unwrap();
    api.wait_cancels(1);
    rig.until("stopping", |v| v.status.phase == Phase::Stopping);
    // The watch starts when the cancel's answer is applied, which a test
    // cannot see: move on until the first poll is made, then the pace is the
    // watcher's own.
    rig.advance_until_status(1);
    for polls in 2..=MAX_STOP_POLLS as usize {
        rig.advance(POLL);
        api.wait_status(polls);
    }
    let v = rig.until("stop offered again", |v| v.status.can_stop);
    assert_eq!(v.status.phase, Phase::Working, "the turn never stopped");
    held.send(Ok(finished())).unwrap();
}

#[test]
fn a_failed_cancel_offers_stop_again_at_once() {
    let (api, rig, stream) = fresh();
    let held = api.gate_prompt();
    api.reply_cancel(Err(Error::Io(io::Error::other("reset"))));
    rig.handle.send("tidy").unwrap();
    api.wait_prompts(1);
    Rig::feed(&stream, entry_item(&user(3, "tidy")));
    rig.until("stoppable", |v| v.status.can_stop);
    rig.handle.stop().unwrap();
    api.wait_cancels(1);
    rig.until("offered again", |v| {
        v.status.can_stop && v.status.phase == Phase::Working
    });
    held.send(Ok(finished())).unwrap();
}

#[test]
fn a_steer_note_reaches_the_serve_and_a_failed_one_comes_back() {
    let (api, rig, stream) = fresh();
    let held = api.gate_prompt();
    api.reply_interrupt(Err(Error::Io(io::Error::other("reset"))));
    rig.handle.send("tidy").unwrap();
    api.wait_prompts(1);
    Rig::feed(&stream, entry_item(&user(3, "tidy")));
    rig.until("steerable", |v| v.status.send == SendMode::Steer);
    rig.handle.steer("also the tests").unwrap();
    api.wait_interrupts(1);
    assert_eq!(api.interrupts(), vec!["also the tests"]);
    rig.until("back in the draft", |v| v.draft == "also the tests");
    held.send(Ok(finished())).unwrap();
}
