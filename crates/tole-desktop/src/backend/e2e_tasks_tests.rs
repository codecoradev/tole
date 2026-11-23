//! The open tasks on the backend's threads: recorded while the serve is down,
//! started when it is ready, made again after a restart, capped, and their
//! commands refused with a reason when there is no driver to take them.

use std::sync::mpsc::Sender;
use std::sync::Arc;
use std::time::Duration;

use serde_json::json;
use tole_client::{EntriesPage, PromptReply, StreamItem};
use tole_desktop_model::task::SendMode;
use tole_desktop_model::ServeState;

use super::fake::{restarting, running, FakeLink};
use super::rig::{is_known, is_unknown, known_api, no_approvals, Rig};
use super::{OpenRefused, TaskRefused};
use crate::driver::fake::{FakeApi, Frame};
use crate::driver::task::MAX_OPEN_TASKS;

fn secs(n: u64) -> Duration {
    Duration::from_secs(n)
}

/// The history of a session with nothing in it.
fn empty_page(session: &str) -> EntriesPage {
    serde_json::from_value(json!({
        "id": session, "entries": [], "nextAfter": 0, "hasMore": false, "lastSeq": 0
    }))
    .expect("a page")
}

fn finished() -> PromptReply {
    serde_json::from_value(json!({"stopReason": "end_turn", "text": "ok"})).expect("a reply")
}

/// An api that answers the first loads and has the history of `sessions`.
fn api_with(sessions: &[&str]) -> Arc<FakeApi> {
    let api = known_api(0, 0);
    for s in sessions {
        api.reply_entries(Ok(empty_page(s)));
    }
    for _ in 0..4 {
        api.reply_list(Ok(no_approvals()));
    }
    api
}

/// The serve restarts: not ready, and ready again before anyone looks.
fn restart(link: &FakeLink) {
    link.set(restarting());
    link.set(running("2"));
}

fn up(stream: &Sender<Frame>) {
    stream.send(Frame::Item(StreamItem::Heartbeat)).unwrap();
}

#[test]
fn a_task_opened_while_the_serve_is_down_is_recorded_and_started_when_it_is_ready() {
    let link = FakeLink::new(ServeState::Starting);
    let api = api_with(&["s1"]);
    link.queue_api(api.clone());
    let rig = Rig::start(link.clone());
    rig.backend().open_task("s1").expect("recorded");
    rig.advance(secs(5));
    let v = rig.view();
    assert_eq!(v.open_tasks, ["s1"], "the wish is kept");
    assert!(rig.backend().task_view("s1").is_none(), "no driver yet");
    assert_eq!(link.api_calls(), 0);
    assert_eq!(rig.backend().send("s1", "x"), Err(TaskRefused::Unavailable));
    link.set(running("1"));
    rig.wait_ticking("the driver", |b| b.task_view("s1").is_some());
    api.wait_entries(1);
    assert_eq!(rig.view().open_tasks, ["s1"]);
}

#[test]
fn the_approvals_driver_follows_exactly_the_open_sessions() {
    let link = FakeLink::ready();
    let api = api_with(&[]);
    link.queue_api(api.clone());
    let rig = Rig::start(link.clone());
    rig.until("known", |v| is_known(v, 0, 0));
    assert_eq!(api.opens("s1"), 0, "nothing is open: no stream");
    rig.backend().open_task("s1").unwrap();
    api.wait_opens("s1", 1);
    rig.backend().open_task("s2").unwrap();
    api.wait_opens("s2", 1);
    assert_eq!(api.opens("s3"), 0);
}

#[test]
fn a_task_open_at_the_start_is_followed_from_the_first_look() {
    let link = FakeLink::new(ServeState::Starting);
    let api = api_with(&["s1", "s2"]);
    link.queue_api(api.clone());
    let rig = Rig::start(link.clone());
    rig.backend().open_task("s1").unwrap();
    rig.backend().open_task("s2").unwrap();
    link.set(running("1"));
    rig.wait_ticking("both drivers", |b| {
        b.task_view("s1").is_some() && b.task_view("s2").is_some()
    });
    api.wait_opens("s1", 1);
    api.wait_opens("s2", 1);
    api.wait_entries(2);
}

#[test]
fn the_fourth_task_is_refused_and_a_closed_one_frees_its_place() {
    let link = FakeLink::new(ServeState::Starting);
    let rig = Rig::start(link);
    let b = rig.backend();
    for id in ["a", "b", "c"] {
        b.open_task(id).expect("within the cap");
    }
    let refused = b.open_task("d");
    assert_eq!(
        refused,
        Err(OpenRefused::TooMany {
            max: MAX_OPEN_TASKS
        })
    );
    assert!(refused.unwrap_err().to_string().contains('3'), "a reason");
    assert_eq!(rig.view().open_tasks, ["a", "b", "c"]);
    assert!(b.close_task("b"));
    assert!(!b.close_task("b"), "closed once");
    b.open_task("d").expect("a place is free");
    assert_eq!(rig.view().open_tasks, ["a", "c", "d"]);
}

#[test]
fn opening_an_open_task_is_fine_and_takes_no_place() {
    let link = FakeLink::new(ServeState::Starting);
    let rig = Rig::start(link);
    let b = rig.backend();
    for _ in 0..10 {
        b.open_task("a").expect("again");
    }
    b.open_task("b").unwrap();
    b.open_task("c").unwrap();
    assert_eq!(rig.view().open_tasks, ["a", "b", "c"]);
}

#[test]
fn an_id_the_serve_cannot_have_given_is_refused_and_nothing_is_started() {
    let link = FakeLink::ready();
    let api = api_with(&[]);
    link.queue_api(api.clone());
    let rig = Rig::start(link);
    rig.until("known", |v| is_known(v, 0, 0));
    let hostile = [
        String::new(),
        "a/b".into(),
        "a b".into(),
        "a\nb".into(),
        "../x".into(),
        "é".into(),
        "x".repeat(65),
        "x".repeat(1 << 20),
    ];
    for id in &hostile {
        assert_eq!(rig.backend().open_task(id), Err(OpenRefused::InvalidId));
    }
    rig.backend()
        .open_task(&"x".repeat(64))
        .expect("64 bytes is the limit");
    assert_eq!(rig.view().open_tasks.len(), 1);
    assert!(!rig.backend().close_task("a/b"));
}

#[test]
fn a_serve_restart_with_three_open_tasks_makes_the_drivers_again_for_the_same_sessions() {
    let link = FakeLink::ready();
    let first = api_with(&["s1", "s2", "s3"]);
    let second = api_with(&["s1", "s2", "s3"]);
    link.queue_api(first.clone());
    link.queue_api(second.clone());
    let rig = Rig::start(link.clone());
    for id in ["s1", "s2", "s3"] {
        rig.backend().open_task(id).unwrap();
    }
    first.wait_entries(3);
    for id in ["s1", "s2", "s3"] {
        rig.wait("a driver", |b| b.task_view(id).is_some());
    }
    restart(&link);
    for id in ["s1", "s2", "s3"] {
        rig.wait_ticking("a new driver", |b| b.task_view(id).is_some());
        second.wait_opens(id, 1);
    }
    second.wait_entries(3);
    assert_eq!(
        rig.view().open_tasks,
        ["s1", "s2", "s3"],
        "in the same order"
    );
    assert_eq!(
        first.entries_calls().len(),
        3,
        "the old drivers asked no more"
    );
}

#[test]
fn a_task_closed_during_a_restart_is_not_started_again() {
    let link = FakeLink::ready();
    let first = api_with(&["s1", "s2", "s3"]);
    let second = api_with(&["s1", "s3"]);
    link.queue_api(first.clone());
    link.queue_api(second.clone());
    let rig = Rig::start(link.clone());
    for id in ["s1", "s2", "s3"] {
        rig.backend().open_task(id).unwrap();
    }
    first.wait_entries(3);
    link.set(restarting());
    rig.until_ticking("unknown", is_unknown);
    assert!(
        rig.backend().close_task("s2"),
        "closed while the serve is down"
    );
    link.set(running("2"));
    for id in ["s1", "s3"] {
        rig.wait_ticking("a driver", |b| b.task_view(id).is_some());
    }
    second.wait_entries(2);
    rig.advance(secs(2));
    assert_eq!(
        second.entries_calls().len(),
        2,
        "no driver for the closed task"
    );
    assert!(rig.backend().task_view("s2").is_none());
    assert_eq!(second.opens("s2"), 0, "and not followed");
    assert_eq!(rig.view().open_tasks, ["s1", "s3"]);
}

#[test]
fn commands_are_refused_with_a_reason_when_no_driver_can_take_them() {
    let link = FakeLink::new(ServeState::Starting);
    let rig = Rig::start(link.clone());
    let b = rig.backend();
    assert_eq!(b.send("s1", "x"), Err(TaskRefused::NotOpen));
    assert_eq!(b.steer("s1", "x"), Err(TaskRefused::NotOpen));
    assert_eq!(b.stop("s1"), Err(TaskRefused::NotOpen));
    b.open_task("s1").unwrap();
    assert_eq!(b.send("s1", "x"), Err(TaskRefused::Unavailable));
    assert_eq!(b.steer("s1", "x"), Err(TaskRefused::Unavailable));
    assert_eq!(b.stop("s1"), Err(TaskRefused::Unavailable));
    let decision = tole_desktop_model::approvals::Decision::AllowOnce;
    assert_eq!(
        b.decide("s1", "a1", decision),
        Err(TaskRefused::Unavailable)
    );
    assert!(b.task_view("s1").is_none());
    assert!(b.with_task("s1", |_| ()).is_none());
}

#[test]
fn a_command_reaches_the_driver_of_the_task() {
    let link = FakeLink::ready();
    let api = api_with(&["s1"]);
    let (a, b) = (api.open_stream("s1"), api.open_stream("s1"));
    api.reply_prompt(Ok(finished()));
    link.queue_api(api.clone());
    let rig = Rig::start(link);
    rig.backend().open_task("s1").unwrap();
    api.wait_opens("s1", 2); // the approvals driver's reader and the task's
    up(&a);
    up(&b);
    rig.wait("send is open", |bk| {
        bk.task_view("s1")
            .is_some_and(|v| v.status.send == SendMode::Send)
    });
    rig.backend().send("s1", "hello").expect("queued");
    api.wait_prompts(1);
    assert_eq!(api.prompts(), ["hello"]);
}

#[test]
fn a_restart_while_a_prompt_is_in_flight_leaves_a_clean_new_driver() {
    let link = FakeLink::ready();
    let first = api_with(&["s1"]);
    let (a, b) = (first.open_stream("s1"), first.open_stream("s1"));
    let held = first.gate_prompt();
    link.queue_api(first.clone());
    let second = api_with(&["s1"]);
    link.queue_api(second.clone());
    let rig = Rig::start(link.clone());
    rig.backend().open_task("s1").unwrap();
    first.wait_opens("s1", 2);
    up(&a);
    up(&b);
    rig.wait("send is open", |bk| {
        bk.task_view("s1")
            .is_some_and(|v| v.status.send == SendMode::Send)
    });
    rig.backend().send("s1", "hello").unwrap();
    first.wait_prompts(1);

    link.set(restarting());
    // Before the worker has looked: no driver of the old serve is lent.
    assert!(rig.backend().task_view("s1").is_none());
    assert_eq!(
        rig.backend().send("s1", "again"),
        Err(TaskRefused::Unavailable)
    );
    rig.wait_ticking("the call is let go", |bk| bk.thread_report().detached >= 1);
    link.set(running("2"));
    rig.wait_ticking("the new driver", |bk| bk.task_view("s1").is_some());
    second.wait_entries(1);

    // The old call answers now, to nobody: nothing changes for the new driver.
    held.send(Ok(finished())).unwrap();
    rig.advance(secs(1));
    assert!(second.prompts().is_empty(), "the words were not sent again");
    assert!(rig.backend().task_view("s1").is_some());
    assert!(rig.backend().is_running());
}
