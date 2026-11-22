//! Findings of the review: the shapes core really writes, a busy that is not this
//! task's call, the approvals of other owners, and a history that lies.

use super::*;
use crate::adapter::task::{
    approvals_refreshed, history_page, prompt_outcome, stop_step, BusyPolling, PageStep,
    PromptOutcome, ResumePolling, MAX_REFRESH_ROWS, MAX_STOP_POLLS, RESUME_POLL, SLOW_BUSY_POLL,
};
use std::time::Duration;
use tole_client::{CancelReply, EntriesPage, Error, SessionStatus};
use tole_desktop_model::task::{Enter, EnterOutcome, Phase};

/// An `error` as `append_turn_error` writes it: a child of the turn's user message.
fn turn_error(seq: u64, parent: &str, error: &str, detail: &str) -> Entry {
    let payload = json!({"error": error, "detail": detail});
    entry(seq, &format!("e_{seq}"), Some(parent), "error", payload)
}

fn note(seq: u64, text: &str) -> Entry {
    let payload = json!({"role": "user", "text": text, "denial_note": true});
    entry(seq, &format!("e_{seq}"), None, "message", payload)
}

fn status(busy: bool) -> Result<SessionStatus, Error> {
    let row = json!({"id": "s1", "entries": null, "busy": busy, "usage_report": {},
        "model": null, "effort": null, "contextWindow": null, "contextUsed": null,
        "name": null, "snapshotAt": null, "stale": false});
    Ok(serde_json::from_value(row).expect("a status"))
}

fn cancel(cancelled: bool) -> Result<CancelReply, Error> {
    let row = json!({"cancelled": cancelled, "sessionId": "s1"});
    Ok(serde_json::from_value(row).expect("a cancel reply"))
}

fn phase(t: &Task) -> Phase {
    t.state.status().phase
}

fn http(status: u16) -> Error {
    Error::Status {
        status,
        message: String::new(),
        body: json!(null),
    }
}

fn press(t: &mut Task, text: &str) -> tole_desktop_model::task::Submission {
    t.state.composer.text = text.into();
    let enter = Enter {
        shift: false,
        composing: false,
    };
    match t.state.press_enter(enter) {
        EnterOutcome::Submit(s) => s,
        other => panic!("{other:?}"),
    }
}

// --- turn errors are not settlements -----------------------------------------

#[test]
fn a_stop_on_a_read_only_turn_still_lets_the_status_say_nothing_was_changed() {
    let mut t = Task::new();
    t.entries(&[
        user(1, "look around"),
        intent(2, "read_file", json!({"path": "a.rs"})),
        result(3, 2, json!("x")),
        turn_error(
            4,
            "e_1",
            "cancelled",
            "client cancelled the turn (session/cancel)",
        ),
    ]);
    t.state.history_complete = true;
    let counts = t.state.status().counts;
    assert_eq!((counts.read, counts.changed, counts.unsure), (1, 0, 0));
}

#[test]
fn one_denied_approval_and_the_other_turn_errors_settle_no_call() {
    let mut t = Task::new();
    t.entries(&[
        user(1, "write it"),
        // A denied call leaves no intent: the error is under the user message.
        turn_error(2, "e_1", "approval required", "write_file needs approval"),
        note(3, "[tole] Your call to `write_file` was denied (owner)"),
        turn_error(4, "e_1", "provider failed", "503"),
        turn_error(5, "e_1", "budget exhausted", "steps"),
    ]);
    assert_eq!(t.state.transcript.counts.unsure, 0);
    assert_eq!(t.shape(), "U N");
}

#[test]
fn a_failed_write_still_counts_as_may_have_changed() {
    let mut t = Task::new();
    t.entries(&[
        user(1, "write it"),
        intent(2, "write_file", json!({"path": "a.txt"})),
        failure(3, 2),
    ]);
    assert_eq!(t.state.transcript.counts.unsure, 1);
    // A turn error after it does not hide that.
    t.entries(&[turn_error(4, "e_1", "cancelled", "stop")]);
    assert_eq!(t.state.transcript.counts.unsure, 1);
}

#[test]
fn a_turn_error_is_known_by_its_parent_or_by_its_shape_and_anything_else_settles() {
    let mut m = EntryMapper::new();
    // The parent was never seen, but the payload is the turn-error shape.
    let shaped = turn_error(9, "gone", "cancelled", "stop");
    assert!(m.entry(&shaped).is_empty());
    // A user message seen by the mapper is a turn, whatever the payload says.
    m.entry(&user(1, "go"));
    let bare = entry(2, "x", Some("e_1"), "error", json!({"error": "boom"}));
    assert!(m.entry(&bare).is_empty());
    // An unknown parent without the shape is a call the mapper has not seen.
    let unknown = entry(3, "y", Some("i77"), "error", json!({"error": "boom"}));
    assert_eq!(m.entry(&unknown).len(), 1);
    // A tool failure carries `ok`, so its shape is never a turn's.
    let tool = entry(
        4,
        "z",
        Some("i78"),
        "error",
        json!({"ok": false, "error": "b", "detail": 1}),
    );
    assert_eq!(m.entry(&tool).len(), 1);
}

// --- notices are not the owner's words ---------------------------------------

fn interrupt(seq: u64, source: Option<&str>, text: &str) -> Entry {
    let mut payload = json!({"role": "user", "text": text, "interrupt": true, "urgent": false});
    if let Some(s) = source {
        payload["source"] = json!(s);
    }
    entry(seq, &format!("e_{seq}"), None, "message", payload)
}

#[test]
fn a_sibling_notice_with_a_bidi_file_name_is_a_cleaned_note_not_the_owners_steer() {
    let mut t = Task::new();
    let text = "[sibling notice] wrote \"\u{202E}gnp.exe\"";
    t.entries(&[user(1, "go"), interrupt(2, Some("notice"), text)]);
    assert_eq!(t.shape(), "U N");
    let Some(Item::Note(n)) = t.state.transcript.items.last() else {
        panic!()
    };
    assert!(n.contains("gnp.exe") && !n.contains('\u{202E}'), "{n}");
}

#[test]
fn only_an_owner_interrupt_is_a_steer() {
    for source in [Some("owner"), None] {
        let mut t = Task::new();
        t.entries(&[user(1, "go"), interrupt(2, source, "faster")]);
        assert_eq!(t.shape(), "U s", "{source:?}");
    }
    for source in ["notice", "job", ""] {
        let mut t = Task::new();
        t.entries(&[user(1, "go"), interrupt(2, Some(source), "x")]);
        assert_eq!(t.shape(), "U N", "{source:?}");
    }
    let mut odd = interrupt(2, None, "x");
    odd.payload["source"] = json!(7);
    let mut t = Task::new();
    t.entries(&[user(1, "go"), odd]);
    assert_eq!(t.shape(), "U N");
}

// --- a busy that is not this task's call -------------------------------------

#[test]
fn a_409_polls_the_status_until_the_other_turn_is_over() {
    let mut t = Task::new();
    let sent = press(&mut t, "go");
    let PromptOutcome::Failed {
        server_busy,
        check_status,
    } = prompt_outcome(&Err(http(409)))
    else {
        panic!()
    };
    assert!(server_busy && check_status);
    t.state.send_failed(sent, server_busy);
    assert_eq!(phase(&t), Phase::Working);
    let mut poll = BusyPolling::foreign();
    for _ in 0..3 {
        let step = poll.status(&status(true));
        assert!(!step.finished);
        step.apply(&mut t.state);
        assert_eq!(phase(&t), Phase::Working);
    }
    // The other face's turn ended with no answer entry (a cancel).
    let step = poll.status(&status(false));
    assert!(step.finished);
    step.apply(&mut t.state);
    assert_eq!(phase(&t), Phase::Idle);
}

#[test]
fn a_stop_after_a_409_is_not_stranded_in_stopping() {
    let mut t = Task::new();
    let sent = press(&mut t, "go");
    t.state.send_failed(sent, true);
    assert!(t.state.press_stop());
    // serve's cancel says true for any known session, idle or not.
    let step = stop_step(&cancel(true));
    step.apply(&mut t.state);
    assert_eq!(phase(&t), Phase::Stopping);
    let mut poll = BusyPolling::after_stop();
    poll.status(&status(true)).apply(&mut t.state);
    assert_eq!(phase(&t), Phase::Stopping);
    let step = poll.status(&status(false));
    assert!(step.finished && !step.stop_failed);
    step.apply(&mut t.state);
    assert_eq!(phase(&t), Phase::Idle);
}

#[test]
fn a_stop_the_server_never_honours_is_offered_again() {
    let mut t = Task::new();
    t.live(&[ready(true)]);
    assert!(t.state.press_stop());
    let mut poll = BusyPolling::after_stop();
    for _ in 1..MAX_STOP_POLLS {
        let step = poll.status(&status(true));
        assert!(!step.finished);
        step.apply(&mut t.state);
    }
    assert_eq!(poll.interval(), RESUME_POLL);
    let step = poll.status(&status(true));
    assert!(!step.finished && step.stop_failed);
    step.apply(&mut t.state);
    assert_eq!(phase(&t), Phase::Working);
    assert!(t.state.press_stop(), "Stop works again");
    // The watch goes on, slower: the foreign turn that ends later still ends the task.
    assert_eq!(poll.interval(), SLOW_BUSY_POLL);
    let more = poll.status(&status(true));
    assert!(!more.finished && !more.stop_failed);
    let end = poll.status(&status(false));
    assert!(end.finished);
    end.apply(&mut t.state);
    assert_eq!(phase(&t), Phase::Idle);
}

fn lost() -> Result<SessionStatus, Error> {
    Err(Error::Status {
        status: 503,
        message: String::new(),
        body: json!(null),
    })
}

#[test]
fn a_foreign_polling_slows_down_after_failures_and_one_answer_resets_the_count() {
    let mut poll = BusyPolling::foreign();
    for _ in 0..4 {
        let step = poll.status(&lost());
        assert!(step.events.is_empty() && !step.finished && !step.stop_failed);
    }
    assert!(!poll.status(&status(true)).finished);
    for _ in 0..4 {
        poll.status(&lost());
    }
    assert_eq!(poll.interval(), RESUME_POLL);
    let slow = poll.status(&lost());
    assert!(
        !slow.finished && !slow.stop_failed,
        "a foreign turn has no Stop to give back"
    );
    assert_eq!(poll.interval(), SLOW_BUSY_POLL);
}

#[test]
fn a_polling_that_never_gets_an_answer_ends_with_a_lost_connection_not_a_silent_working() {
    let mut t = Task::new();
    t.live(&[ready(true)]);
    let mut poll = BusyPolling::foreign();
    let mut steps = 0;
    loop {
        let step = poll.status(&lost());
        steps += 1;
        if step.finished {
            assert_eq!(step.events, [TaskEvent::ConnectionLost]);
            step.apply(&mut t.state);
            break;
        }
        assert!(steps < 100, "it must end");
    }
    assert_eq!(phase(&t), Phase::Offline);
}

#[test]
fn failed_status_calls_after_a_stop_give_it_back_and_keep_watching() {
    let mut t = Task::new();
    t.live(&[ready(true)]);
    assert!(t.state.press_stop());
    let mut poll = BusyPolling::after_stop();
    for _ in 0..4 {
        let step = poll.status(&lost());
        assert!(!step.stop_failed);
        step.apply(&mut t.state);
    }
    assert_eq!(phase(&t), Phase::Stopping);
    let step = poll.status(&lost());
    assert!(step.stop_failed && !step.finished);
    step.apply(&mut t.state);
    assert_eq!(phase(&t), Phase::Working);
    assert!(t.state.press_stop(), "the owner can Stop again");
    // Not given back twice.
    assert!(!poll.status(&lost()).stop_failed);
}

#[test]
fn failed_polls_never_end_an_allowed_turn_the_stream_already_showed_running() {
    let mut t = Task::new();
    t.live(&[
        approval_frame("approval_requested", "a1", "pending"),
        approval_frame("approval_resolved", "a1", "approved"),
        ready(true),
    ]);
    assert_eq!(phase(&t), Phase::Working);
    let mut poll = ResumePolling::new();
    poll.activity();
    let lost = || Err(Error::Io(std::io::ErrorKind::ConnectionReset.into()));
    for i in 0..10u64 {
        let step = poll.status(&lost(), Duration::from_secs(i * 3));
        assert!(!step.finished && step.events.is_empty());
    }
    assert_eq!(phase(&t), Phase::Working);
    let step = poll.status(&status(false), Duration::from_secs(40));
    assert!(step.finished);
    t.apply(step.events);
    assert_eq!(phase(&t), Phase::Idle);
}

#[test]
fn an_approval_decided_between_two_polls_still_resumes_the_model() {
    let mut t = Task::new();
    t.live(&[
        approval_frame("approval_requested", "a9", "approved"),
        approval_frame("approval_resolved", "a9", "approved"),
    ]);
    assert_eq!(phase(&t), Phase::Resuming);
    assert!(t.state.run.approvals.is_empty());
    let mut denied = Task::new();
    denied.live(&[
        approval_frame("approval_requested", "a9", "denied"),
        approval_frame("approval_resolved", "a9", "denied"),
    ]);
    assert_eq!(phase(&denied), Phase::Idle);
}

// --- the approvals of this task ----------------------------------------------

fn row(id: &str, session: Option<&str>) -> tole_client::Approval {
    let mut r = json!({"id": id, "tool": "write_file", "status": "pending"});
    if let Some(s) = session {
        r["sessionId"] = json!(s);
    }
    serde_json::from_value(r).expect("an approval")
}

#[test]
fn a_delegated_childs_row_is_not_this_tasks_approval() {
    // `GET /approvals` with no `?session=` appends `dap-*` rows with no session.
    let dap: tole_client::Approval = serde_json::from_value(json!({
        "id": "dap-1f", "tool": "write_file", "status": "pending",
        "source": "delegation", "fingerprint": "fp"}))
    .unwrap();
    let rows = [dap, row("apr-1", Some("s1")), row("apr-2", Some("other"))];
    let mut t = Task::new();
    t.apply(approvals_refreshed(&rows, "s1").into_iter().collect());
    let ids: Vec<_> = t
        .state
        .run
        .approvals
        .iter()
        .map(|a| a.id.as_str())
        .collect();
    assert_eq!(ids, ["apr-1"]);
    let mut only_dap = Task::new();
    only_dap.apply(approvals_refreshed(&rows[..1], "s1").into_iter().collect());
    assert_eq!(
        phase(&only_dap),
        Phase::Idle,
        "no lock on a foreign approval"
    );
}

#[test]
fn a_queue_over_the_cap_is_not_claimed_as_complete() {
    let rows = |n: usize| -> Vec<_> { (0..n).map(|i| row(&format!("a{i}"), Some("s1"))).collect() };
    let ok = approvals_refreshed(&rows(MAX_REFRESH_ROWS), "s1");
    assert!(
        matches!(ok, Some(TaskEvent::ApprovalsRefreshed { pending }) if pending.len() == MAX_REFRESH_ROWS)
    );
    assert_eq!(approvals_refreshed(&rows(MAX_REFRESH_ROWS + 1), "s1"), None);
    // Rows of others do not count against the cap.
    let mut many = rows(3);
    many.extend((0..2000).map(|i| row(&format!("o{i}"), Some("other"))));
    assert!(approvals_refreshed(&many, "s1").is_some());
    // A refused refresh leaves the model asking for one.
    let mut t = Task::new();
    t.live(&[StreamItem::Reconnected { after: 0 }]);
    assert!(approvals_refreshed(&rows(MAX_REFRESH_ROWS + 1), "s1").is_none());
    assert!(t.state.run.approvals_stale);
}

// --- a history that lies -----------------------------------------------------

fn raw_page(entries: Vec<Entry>, next_after: u64, has_more: bool, last_seq: u64) -> EntriesPage {
    let entries = serde_json::to_value(entries).unwrap();
    let row = json!({"id": "s1", "entries": entries, "nextAfter": next_after,
        "hasMore": has_more, "lastSeq": last_seq});
    serde_json::from_value(row).expect("a page")
}

fn step_of(page: &EntriesPage, after: u64) -> PageStep {
    history_page(&mut EntryMapper::new(), page, after).step
}

#[test]
fn a_cursor_short_of_last_seq_is_not_a_whole_history() {
    // Nothing between 10 and 5000 was read.
    let lie = raw_page(vec![], 1000, false, 5000);
    assert_eq!(step_of(&lie, 10), PageStep::Stalled);
    let honest_empty = raw_page(vec![], 10, false, 10);
    assert_eq!(step_of(&honest_empty, 10), PageStep::Complete);
    let none = raw_page(vec![], 0, false, 0);
    assert_eq!(step_of(&none, 0), PageStep::Complete);
    // Entries came in since the page was cut: read on.
    let behind = raw_page(vec![user(11, "x")], 11, false, 14);
    assert_eq!(step_of(&behind, 10), PageStep::More { next_after: 11 });
}

#[test]
fn pages_that_promise_more_and_give_nothing_stall() {
    let creeping = raw_page(vec![], 11, true, 99);
    assert_eq!(step_of(&creeping, 10), PageStep::Stalled);
    let same = raw_page(vec![], 10, true, 99);
    assert_eq!(step_of(&same, 10), PageStep::Stalled);
}

#[test]
fn entries_out_of_order_or_not_above_the_cursor_or_off_the_cursor_stall() {
    let unordered = raw_page(vec![user(5, "a"), user(4, "b")], 4, true, 99);
    assert_eq!(step_of(&unordered, 0), PageStep::Stalled);
    let below = raw_page(vec![user(3, "a")], 3, true, 99);
    assert_eq!(step_of(&below, 5), PageStep::Stalled);
    let off = raw_page(vec![user(3, "a")], 9, true, 99);
    assert_eq!(step_of(&off, 0), PageStep::Stalled);
    let good = raw_page(vec![user(3, "a"), user(4, "b")], 4, true, 99);
    assert_eq!(step_of(&good, 0), PageStep::More { next_after: 4 });
}

// --- a plan core refuses -----------------------------------------------------

#[test]
fn an_update_plan_is_refused_whole_when_any_entry_is_bad_even_past_the_copy_cap() {
    let mut entries: Vec<Value> = (0..200)
        .map(|i| json!({"content": format!("t{i}")}))
        .collect();
    entries.push(json!({"content": "x", "status": "someday"}));
    let mut t = Task::new();
    t.entries(&[
        user(1, "go"),
        intent(2, "update_plan", json!({"entries": entries})),
    ]);
    assert_eq!(t.shape(), "U", "no plan row");
    // A long valid list is accepted and cut to what the adapter copies.
    let long: Vec<Value> = (0..300)
        .map(|i| json!({"content": format!("t{i}")}))
        .collect();
    let mut ok = Task::new();
    ok.entries(&[
        user(1, "go"),
        intent(2, "update_plan", json!({"entries": long})),
    ]);
    assert_eq!(ok.shape(), "U P");
}
