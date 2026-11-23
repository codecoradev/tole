//! Starting a task: the session is made on the serve in the folder asked for,
//! opened as a task when it worked, and left unopened and reported when it did
//! not; nothing is asked of the serve when it cannot be answered.

use serde_json::json;
use tole_client::{EntriesPage, SessionOpened};
use tole_desktop_model::ServeState;

use super::fake::{running, FakeLink};
use super::new_task::NewTaskRefused;
use super::rig::{is_known, known_api, no_approvals, Rig};
use crate::driver::task::MAX_OPEN_TASKS;

fn opened(id: &str) -> SessionOpened {
    serde_json::from_value(json!({"sessionId": id, "workspace": "/w"})).expect("a reply")
}

fn empty_page(session: &str) -> EntriesPage {
    serde_json::from_value(json!({
        "id": session, "entries": [], "nextAfter": 0, "hasMore": false, "lastSeq": 0
    }))
    .expect("a page")
}

#[test]
fn a_new_task_is_made_in_the_folder_asked_for_and_opens_with_a_driver() {
    let link = FakeLink::ready();
    let api = known_api(0, 0);
    api.reply_create(Ok(opened("s9")));
    api.reply_entries(Ok(empty_page("s9")));
    for _ in 0..4 {
        api.reply_list(Ok(no_approvals()));
    }
    link.queue_api(api.clone());
    let rig = Rig::start(link);
    rig.until("known", |v| is_known(v, 0, 0));
    assert!(rig.backend().open_tasks().is_empty());

    rig.backend()
        .new_task(Some("/w/Invoices"))
        .expect("started");
    api.wait_creates(1);
    assert_eq!(api.create_calls(), [Some("/w/Invoices".to_owned())]);
    rig.until("the task is open", |v| v.open_tasks == ["s9"]);
    assert!(!rig.backend().new_task_failed());
    rig.wait_ticking("its driver", |b| b.task_view("s9").is_some());
}

#[test]
fn a_task_the_serve_could_not_make_opens_nothing_and_the_next_try_clears_the_report() {
    let link = FakeLink::ready();
    let api = known_api(0, 0);
    // Nothing is scripted for the first call: it fails as a refused connection.
    link.queue_api(api.clone());
    let rig = Rig::start(link);
    rig.until("known", |v| is_known(v, 0, 0));

    rig.backend().new_task(None).expect("started");
    api.wait_creates(1);
    rig.wait("the failure is reported", |b| b.new_task_failed());
    assert!(rig.backend().open_tasks().is_empty(), "nothing was opened");
    assert_eq!(api.create_calls(), [None], "the serve's own workspace");

    // The next try clears the report at once, before the serve answers.
    let answer = api.gate_create();
    api.reply_entries(Ok(empty_page("s1")));
    rig.backend().new_task(None).expect("started again");
    assert!(
        !rig.backend().new_task_failed(),
        "cleared while the call is out"
    );
    api.wait_creates(2);
    answer.send(Ok(opened("s1"))).expect("the call waits");
    rig.until("the second one opens", |v| v.open_tasks == ["s1"]);
    assert!(!rig.backend().new_task_failed());
}

#[test]
fn nothing_is_asked_of_the_serve_when_it_is_not_ready_or_three_tasks_are_open() {
    let link = FakeLink::new(ServeState::Starting);
    let api = known_api(0, 0);
    link.queue_api(api.clone());
    let rig = Rig::start(link.clone());
    assert_eq!(rig.backend().new_task(None), Err(NewTaskRefused::NotReady));

    for id in ["a", "b", "c"] {
        rig.backend().open_task(id).expect("within the cap");
    }
    link.set(running("1"));
    rig.wait_ticking("attached", |b| b.task_view("a").is_some());
    let refused = rig.backend().new_task(None);
    assert_eq!(
        refused,
        Err(NewTaskRefused::TooMany {
            max: MAX_OPEN_TASKS
        })
    );
    assert!(refused.unwrap_err().to_string().contains('3'), "a reason");
    assert!(api.create_calls().is_empty(), "the serve was not asked");
}

#[test]
fn two_calls_at_once_make_one_session_and_the_place_is_free_again_when_it_is_done() {
    let link = FakeLink::ready();
    let api = known_api(0, 0);
    let answer = api.gate_create();
    api.reply_entries(Ok(empty_page("s1")));
    for _ in 0..4 {
        api.reply_list(Ok(no_approvals()));
    }
    link.queue_api(api.clone());
    let rig = Rig::start(link);
    rig.until("known", |v| is_known(v, 0, 0));

    rig.backend().new_task(None).expect("the first is started");
    api.wait_creates(1);
    // The first is still out on the serve: a second click starts nothing.
    for _ in 0..5 {
        assert_eq!(rig.backend().new_task(None), Err(NewTaskRefused::Busy));
    }
    assert_eq!(api.create_calls().len(), 1, "one POST /sessions");
    answer.send(Ok(opened("s1"))).expect("the call waits");
    rig.until("the task opens", |v| v.open_tasks == ["s1"]);
    // Done: the place is free again, and the next call is taken.
    let next = std::cell::Cell::new(Err(NewTaskRefused::Busy));
    rig.wait("the place to be free", |b| {
        next.set(b.new_task(None));
        next.get() != Err(NewTaskRefused::Busy)
    });
    assert_eq!(next.get(), Ok(()));
    api.wait_creates(2);
}

#[test]
fn the_place_is_freed_when_the_serve_fails_and_when_the_call_panics() {
    let link = FakeLink::ready();
    let api = known_api(0, 0);
    link.queue_api(api.clone());
    let rig = Rig::start(link);
    rig.until("known", |v| is_known(v, 0, 0));

    // Failure: nothing scripted, the call is refused.
    rig.backend().new_task(None).expect("started");
    rig.wait("the failure", |b| b.new_task_failed());
    // The flag is written just before the place is freed: a try right after it
    // may still find the place held for an instant.
    let started = |rig: &Rig| {
        let result = std::cell::Cell::new(Err(NewTaskRefused::Busy));
        rig.wait("the place to be free", |b| {
            result.set(b.new_task(None));
            result.get() != Err(NewTaskRefused::Busy)
        });
        result.get()
    };
    // The place is free as soon as the failure is reported.
    api.panic_create();
    assert_eq!(started(&rig), Ok(()), "a try after a failure is taken");
    // The panic ends that call; the place is free and the flag says it failed.
    api.wait_creates(2);
    rig.wait("the panic to be reported", |b| b.new_task_failed());
    api.reply_create(Ok(opened("s5")));
    api.reply_entries(Ok(empty_page("s5")));
    assert_eq!(started(&rig), Ok(()), "a try after a panic is taken");
    rig.until("the task opens", |v| v.open_tasks == ["s5"]);
    assert!(!rig.backend().new_task_failed());
}
