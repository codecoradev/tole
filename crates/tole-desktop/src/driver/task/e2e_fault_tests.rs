//! Faults on the threads: a lost stream, a panic in a call, a parked session,
//! a full inbox, shutdown.

use std::io;
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use tole_client::Error;
use tole_desktop_model::approvals::Decision;
use tole_desktop_model::task::{Phase, SendMode};

use super::engine::{Input, TICK_EVERY};
use super::fixtures::*;
use super::rig::{fresh, Rig, PATIENCE};
use super::{Config, LinkState};
use crate::driver::fake::{FakeApi, Frame};

#[test]
fn a_reconnect_reads_this_tasks_queue_again_and_shows_what_it_holds() {
    let (api, rig, stream) = fresh();
    Rig::feed(&stream, reconnected());
    api.wait_list_calls(2);
    assert_eq!(api.list_sessions()[1], Some(S.to_string()));
    api.reply_list(Ok(list(vec![approval("a1", "pending")])));
    // The queue was read before the reply was scripted: the next read gets it.
    Rig::feed(&stream, reconnected());
    api.wait_list_calls(3);
    let v = rig.until("a card", |v| v.approvals.len() == 1);
    assert_eq!(v.status.phase, Phase::WaitingApproval);
}

#[test]
fn a_stream_that_fails_shows_offline_and_is_opened_again_from_the_last_entry() {
    let (api, rig, stream) = fresh();
    let again = api.open_stream(S);
    Rig::feed(&stream, entry_item(&user(3, "x")));
    stream
        .send(Frame::Fail(Error::Io(io::Error::other("reset"))))
        .unwrap();
    rig.until("offline", |v| v.link == LinkState::Lost);
    assert_eq!(rig.handle.snapshot().status.phase, Phase::Offline);
    // The reader waits one second (on the manual clock), then reopens.
    let end = std::time::Instant::now() + PATIENCE;
    while api.opens(S) < 2 {
        assert!(std::time::Instant::now() < end);
        if rig.clock.wait_sleepers(2, Duration::from_secs(1)) && api.opens(S) < 2 {
            rig.tick();
        }
    }
    assert_eq!(
        api.afters(),
        vec![0, 3],
        "after the last entry it delivered"
    );
    Rig::up(&again);
    rig.until("live again", |v| v.link == LinkState::Live);
    api.wait_list_calls(2);
}

#[test]
fn a_panic_in_the_stream_is_a_lost_stream_not_a_dead_driver() {
    let (_api, rig, stream) = fresh();
    stream.send(Frame::Panic).unwrap();
    rig.until("offline", |v| v.link == LinkState::Lost);
    assert!(rig.handle.is_running());
}

#[test]
fn a_session_the_serve_keeps_refusing_is_parked_and_probed_until_it_opens() {
    let api = FakeApi::new();
    api.reply_list(Ok(list(vec![])));
    let mut cfg = Config::new(S);
    cfg.fresh = true;
    // Three refusals (404 until the task is resumed), each on its own stream.
    for _ in 0..3 {
        let refused = api.open_stream(S);
        refused.send(Frame::Fail(status_code(404))).unwrap();
    }
    let rig = Rig::start(api.clone(), cfg);
    let end = std::time::Instant::now() + PATIENCE;
    while api.opens(S) < 3 {
        assert!(std::time::Instant::now() < end);
        if rig.clock.wait_sleepers(2, Duration::from_secs(1)) {
            rig.clock.advance_monotonic(Duration::from_secs(2));
        }
    }
    rig.until("parked", |v| v.parked);
    let opens = api.opens(S);
    // Probed slowly, not at the retry pace; then it opens.
    let working = api.open_stream(S);
    Rig::up(&working);
    let end = std::time::Instant::now() + PATIENCE;
    while api.opens(S) <= opens {
        assert!(std::time::Instant::now() < end);
        rig.clock.wait_sleepers(2, Duration::from_secs(1));
        rig.clock.advance_monotonic(Duration::from_secs(41));
    }
    rig.until("back", |v| !v.parked && v.link == LinkState::Live);
}

#[test]
fn a_panic_in_a_call_is_that_calls_failure_and_the_driver_goes_on() {
    let (api, rig, stream) = fresh();
    // A prompt that panics gives the words back and the serve is asked.
    api.panic_prompt();
    api.reply_status(Ok(status(false)));
    rig.handle.send("tidy").unwrap();
    api.wait_status(1);
    let v = rig.until("restored", |v| v.draft == "tidy");
    assert_eq!(v.status.phase, Phase::Idle);
    // A cancel that panics offers Stop again.
    let held = api.gate_prompt();
    api.panic_cancel();
    rig.handle.send("again").unwrap();
    api.wait_prompts(2);
    Rig::feed(&stream, entry_item(&user(3, "again")));
    rig.until("stoppable", |v| v.status.can_stop);
    rig.handle.stop().unwrap();
    api.wait_cancels(1);
    rig.until("offered again", |v| {
        v.status.can_stop && v.status.phase == Phase::Working
    });
    held.send(Ok(finished())).unwrap();
    rig.until("idle", |v| {
        v.status.send == tole_desktop_model::task::SendMode::Send
    });
    // An answer that panics reads the queue again.
    Rig::feed(&stream, requested("a1"));
    rig.until("a card", |v| v.approvals.len() == 1);
    api.panic_decide();
    rig.handle.decide("a1", Decision::AllowOnce).unwrap();
    api.wait_decides(1);
    api.wait_list_calls(2);
    assert!(rig.handle.is_running());
}

#[test]
fn a_panic_while_reading_the_history_is_retried_after_the_wait() {
    let api = FakeApi::new();
    api.panic_entries();
    api.reply_entries(Ok(page(vec![user(1, "a")], 0, false, 1)));
    let rig = Rig::start(api.clone(), Config::new(S));
    api.wait_entries(1);
    let end = std::time::Instant::now() + PATIENCE;
    while api.entries_calls().len() < 2 {
        assert!(std::time::Instant::now() < end);
        rig.clock.advance_monotonic(TICK_EVERY);
    }
    rig.until("history", |v| v.history_complete && v.total_rows == 1);
    assert!(rig.handle.is_running());
}

/// A prompt that is never answered while the stream shows its turn ended, and
/// the time (on the manual clock) after which it is given up.
fn stuck_prompt() -> (
    std::sync::Arc<FakeApi>,
    Rig,
    std::sync::mpsc::Sender<Frame>,
    std::sync::mpsc::Sender<tole_client::Result<tole_client::PromptReply>>,
) {
    let (api, rig, stream) = fresh();
    let stuck = api.gate_prompt();
    rig.handle.send("tidy").unwrap();
    api.wait_prompts(1);
    Rig::feed(&stream, entry_item(&user(3, "tidy")));
    Rig::feed(&stream, entry_item(&answer(4, "done")));
    rig.until("idle with the call out", |v| {
        v.status.phase == Phase::Idle && v.status.send == SendMode::Disabled(Phase::Working)
    });
    rig.advance(Duration::from_secs(31));
    rig.until("sending is open again", |v| v.status.send == SendMode::Send);
    (api, rig, stream, stuck)
}

#[test]
fn a_prompt_call_that_never_answers_is_replaced_and_the_next_prompt_goes_out() {
    let (api, rig, _stream, stuck) = stuck_prompt();
    api.reply_prompt(Ok(finished()));
    rig.handle.send("again").unwrap();
    // The first thread is still in its call: another made this one.
    api.wait_prompts(2);
    assert_eq!(api.prompts(), vec!["tidy", "again"]);
    rig.until("the second turn ended", |v| {
        v.status.send == SendMode::Send && v.draft.is_empty()
    });
    // The old call answers at last: nothing of it reaches the model.
    stuck.send(Ok(finished())).unwrap();
    rig.sync();
    let v = rig.handle.snapshot();
    assert_eq!(
        (v.status.phase, v.status.send),
        (Phase::Idle, SendMode::Send)
    );
    assert!(rig.handle.is_running());
}

#[test]
fn an_abandoned_call_is_counted_as_detached_at_shutdown() {
    let (_api, rig, stream, stuck) = stuck_prompt();
    drop(stream);
    let report = rig
        .driver
        .expect("running")
        .shutdown(Duration::from_millis(300));
    // The nine threads and the replacement: the one in the old call is left.
    assert!(report.detached >= 1, "{report:?}");
    assert_eq!(report.joined + report.detached, 10);
    let _ = stuck.send(Ok(finished())); // it ends by itself, sending nothing
}

#[test]
fn stop_and_the_cards_work_while_no_stream_is_up() {
    let api = FakeApi::new();
    api.always_status(status(true));
    api.reply_list(Ok(list(vec![approval("a1", "pending")])));
    let mut cfg = Config::new(S);
    cfg.fresh = true; // its stream stays silent: it never comes up
    let rig = Rig::start(api.clone(), cfg);
    rig.clock.advance_monotonic(Duration::from_secs(5));
    api.wait_status(1);
    api.wait_list_calls(1);
    let v = rig.until("the card", |v| v.approvals.len() == 1);
    assert_eq!(v.status.phase, Phase::WaitingApproval);
    assert!(!v.approvals_stale);
}

#[test]
fn a_thread_that_dies_ends_the_driver_cleanly() {
    let (_api, rig, stream) = fresh();
    drop(stream); // its reader can then be joined at once
    rig.handle.command(Input::ThreadFailed).unwrap();
    rig.until("stopped", |v| v.link == LinkState::Stopped);
    let v = rig.handle.snapshot();
    assert!(!rig.handle.is_running());
    assert!(!v.status.can_stop && v.rows.is_empty() && v.approvals.is_empty());
    assert!(rig.handle.send("x").is_err(), "refused once it ended");
    let report = rig
        .driver
        .expect("running")
        .shutdown(Duration::from_secs(5));
    assert_eq!(report.panicked, 0);
}

#[test]
fn shutdown_joins_the_threads_and_leaves_a_stopped_view() {
    let (api, rig, stream) = fresh();
    drop(stream); // the stream ends cleanly: its reader can be joined
    let handle = rig.handle.clone();
    let report = rig
        .driver
        .expect("running")
        .shutdown(Duration::from_secs(5));
    assert_eq!((report.panicked, report.detached), (0, 0), "{report:?}");
    assert_eq!(report.joined, 9);
    let v = handle.snapshot();
    assert_eq!(v.link, LinkState::Stopped);
    assert!(handle.stop().is_err() && !handle.is_running());
    drop(api);
}

#[test]
fn a_call_that_is_still_out_at_shutdown_is_detached_not_waited_for() {
    let (api, rig, stream) = fresh();
    let held = api.gate_prompt();
    rig.handle.send("tidy").unwrap();
    api.wait_prompts(1);
    drop(stream);
    let report = rig
        .driver
        .expect("running")
        .shutdown(Duration::from_millis(300));
    // The prompt thread is still in its call; every thread is accounted for.
    assert!(report.detached >= 1, "the prompt call: {report:?}");
    assert_eq!(report.joined + report.detached, 9);
    held.send(Ok(finished())).unwrap(); // it ends by itself, sending nothing
}

#[test]
fn a_full_inbox_refuses_commands_and_nothing_is_lost_that_was_accepted() {
    let api = FakeApi::new();
    let gate = Arc::new((Mutex::new(false), Condvar::new()));
    let changes = Arc::new((Mutex::new(0u64), Condvar::new()));
    let mut cfg = Config::new(S);
    cfg.fresh = true;
    let (g, c) = (Arc::clone(&gate), Arc::clone(&changes));
    cfg.on_change = Some(Arc::new(move || {
        // The UI is slow: the worker waits here until the test opens the gate.
        let open = g.1.wait_while(g.0.lock().unwrap(), |open| !*open).unwrap();
        drop(open);
        *c.0.lock().unwrap() += 1;
        c.1.notify_all();
    }));
    let rig = Rig::start_with(api, cfg, changes);
    let mut accepted = 0;
    while rig.handle.toggle_group(0).is_ok() {
        accepted += 1;
        assert!(accepted < 10_000, "the inbox is bounded");
    }
    assert!(accepted >= 256, "at least the inbox's room: {accepted}");
    *gate.0.lock().unwrap() = true;
    gate.1.notify_all();
    // The worker drains the inbox and takes commands again.
    rig.until("drained", |_| rig.handle.toggle_group(0).is_ok());
}

#[test]
fn the_rows_follow_the_viewport_and_a_viewport_that_is_not_a_number_is_ignored() {
    let api = FakeApi::new();
    let entries: Vec<_> = (1..=2_000).map(|n| user(n, "x")).collect();
    api.reply_entries(Ok(page(entries, 0, false, 2_000)));
    let rig = Rig::start(api, Config::new(S));
    let v = rig.until("history", |v| v.history_complete);
    assert_eq!(v.total_rows, 2_000);
    assert!(
        v.rows.len() < 30,
        "a window, not the transcript: {}",
        v.rows.len()
    );
    rig.handle.set_viewport(0.0, 200.0, 0.0);
    let small = rig.handle.snapshot();
    assert!(small.rows.len() <= 5 && small.window.start == 0);
    rig.handle.set_viewport(f32::NAN, 1e9, 0.0);
    assert_eq!(rig.handle.snapshot().rows.len(), small.rows.len());
    rig.handle.set_viewport(64.0 * 1_000.0, 200.0, 0.0);
    let far = rig.handle.snapshot();
    assert!(far.window.start >= 990 && far.rows.len() <= 5);
}
