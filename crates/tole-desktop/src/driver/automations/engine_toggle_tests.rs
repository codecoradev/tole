//! The switch: what is sent, what is shown meanwhile, what an answer does and
//! how it meets a read of the list that was out.

use tole_desktop_model::{Notice, Switch, ToggleIntent};

use super::engine::Input;
use super::fixtures::*;

/// The state of the row `job` as the page shows it.
fn switch(t: &T, job: &str) -> Switch {
    t.page().row(&id(job)).expect("a row").switch
}

/// A known page, a moment later (the pace of one call a second is over).
fn calm() -> T {
    let mut t = T::known();
    t.after(5_000);
    t
}

#[test]
fn a_press_sends_one_request_and_the_row_changes_only_when_the_serve_answers() {
    let mut t = calm();
    let out = t.press("off", true);
    let (call, request) = toggle_of(&out).expect("a resume");
    assert_eq!(
        (request.id.as_str(), request.intent),
        ("off", ToggleIntent::Resume)
    );
    assert_eq!(switch(&t, "off"), Switch::Off, "no optimistic flip");
    assert_eq!(t.view().toggling, Some(id("off")));
    assert!(t.notice().is_none());

    let out = t.step(Input::Toggled(call, request, toggled_to("off", "active")));
    assert_eq!(switch(&t, "off"), Switch::On);
    assert_eq!(t.view().toggling, None);
    assert!(load_of(&out).is_some(), "the list is read again at once");
}

#[test]
fn the_same_press_twice_and_a_press_during_a_call_are_one_call() {
    let mut t = calm();
    let (call, request) = toggle_of(&t.press("off", true)).expect("a resume");
    for press in [
        t.press("off", true),
        t.press("on", false),
        t.press("off", false),
    ] {
        assert!(
            toggle_of(&press).is_none() && !press.changed,
            "not queued, not sent"
        );
    }
    t.step(Input::Toggled(call, request, toggled_to("off", "active")));
    let again = t.press("off", true);
    assert!(toggle_of(&again).is_none(), "already on: nothing to send");
    assert!(!again.changed);
}

#[test]
fn a_pause_is_a_pause_and_the_state_shown_is_the_one_the_serve_names() {
    let mut t = calm();
    let (call, request) = toggle_of(&t.press("on", false)).expect("a pause");
    assert_eq!(request.intent, ToggleIntent::Pause);
    t.step(Input::Toggled(call, request, toggled_to("on", "paused")));
    assert_eq!(switch(&t, "on"), Switch::Off);
    // The serve answers with a state the model does not know: not on.
    let (call, request) = toggle_of(&t.press("off", true)).expect("a resume");
    t.step(Input::Toggled(call, request, toggled_to("off", "running")));
    assert_eq!(switch(&t, "off"), Switch::Off);
}

#[test]
fn a_job_tole_made_is_never_sent_and_says_what_the_owner_must_do() {
    let mut t = calm();
    let out = t.press("tole", true);
    assert!(toggle_of(&out).is_none() && out.changed);
    assert_eq!(
        t.notice(),
        Some(Notice::NeedsOwnerReview {
            name: "job tole".into()
        })
    );
    assert!(t.view().toggling.is_none());
}

#[test]
fn a_press_the_page_cannot_vouch_for_is_not_sent() {
    let mut t = T::new();
    t.start();
    let out = t.press("on", false);
    assert!(toggle_of(&out).is_none());
    assert_eq!(
        t.notice(),
        Some(Notice::NotReached),
        "the list is not known"
    );

    let mut t = calm();
    let out = t.press("nobody", true);
    assert!(toggle_of(&out).is_none());
    assert_eq!(t.notice(), Some(Notice::Gone));

    let mut t = calm();
    t.jump(3_600_000);
    assert!(toggle_of(&t.press("on", false)).is_none(), "stale rows");
    assert_eq!(t.notice(), Some(Notice::NotReached));
}

#[test]
fn a_refused_resume_is_a_state_with_an_owner_action_not_an_error_to_retry() {
    let mut t = calm();
    let (call, request) = toggle_of(&t.press("off", true)).expect("a resume");
    let refusal = status_error(403, "not reviewed: the job was not saved by its owner");
    t.step(Input::Toggled(call, request, Err(refusal)));
    assert!(matches!(t.notice(), Some(Notice::NeedsOwnerReview { .. })));
    assert!(t.page().row(&id("off")).expect("row").awaiting_review);
    let again = t.press("off", true);
    assert!(
        toggle_of(&again).is_none(),
        "the next press is not sent either"
    );
}

#[test]
fn a_plan_mode_server_refuses_resumes_and_still_pauses() {
    let mut t = calm();
    let (call, request) = toggle_of(&t.press("off", true)).expect("a resume");
    let refusal = status_error(409, "plan mode");
    t.step(Input::Toggled(call, request, Err(refusal)));
    assert_eq!(t.notice(), Some(Notice::ServerPlanMode));
    assert!(toggle_of(&t.press("off", true)).is_none());
    assert!(
        toggle_of(&t.press("on", false)).is_some(),
        "a pause still goes"
    );
}

#[test]
fn a_failed_call_is_not_repeated_and_the_row_stays() {
    let mut t = calm();
    let (call, request) = toggle_of(&t.press("on", false)).expect("a pause");
    let out = t.step(Input::Toggled(call, request, Err(io_error())));
    assert_eq!(t.notice(), Some(Notice::NotReached));
    assert_eq!(switch(&t, "on"), Switch::On);
    assert!(toggle_of(&out).is_none(), "never twice for one press");
    assert!(
        load_of(&out).is_some(),
        "the call may have landed: look again"
    );
    for _ in 0..40 {
        assert!(toggle_of(&t.after(1_000)).is_none());
    }
}

#[test]
fn an_answer_about_another_job_flips_nothing() {
    let mut t = calm();
    let (call, request) = toggle_of(&t.press("off", true)).expect("a resume");
    t.step(Input::Toggled(call, request, toggled_to("on", "active")));
    assert_eq!(switch(&t, "off"), Switch::Off);
    assert_eq!(t.notice(), Some(Notice::NotReached));
}

/// The race: a list call is out when the switch is answered. Its answer may
/// have been read before the change, and must not put the old state back.
#[test]
fn a_list_read_that_was_out_when_the_switch_was_answered_does_not_undo_it() {
    let mut t = T::known();
    let (old_read, _) = t.next_load(40_000).expect("a refresh is out");
    let (call, request) = toggle_of(&t.press("off", true)).expect("a resume");
    let out = t.step(Input::Toggled(call, request, toggled_to("off", "active")));
    assert_eq!(switch(&t, "off"), Switch::On);
    assert!(load_of(&out).is_none(), "the slot is held by the old read");

    // The old read says `off` is paused, as it was when it began.
    let out = t.step(Input::Jobs(old_read, jobs_reply(three())));
    assert_eq!(
        switch(&t, "off"),
        Switch::On,
        "the stale list is not applied"
    );
    assert!(load_of(&out).is_none(), "one call a second");
    let fresh = load_of(&t.after(1_000)).expect("a read that began after the switch");

    let mut after = three();
    after.jobs[1].state = "active".into();
    after.jobs[1].next_run = Some(1_700_000_000);
    t.step(Input::Jobs(fresh, jobs_reply(after)));
    assert_eq!(switch(&t, "off"), Switch::On);
    assert_eq!(t.view().counts(), Some((2, 1)));
}

/// The other order: the list answers first (with the state before the
/// switch), then the switch answer arrives and wins.
#[test]
fn a_list_that_answers_before_the_switch_does_not_hide_the_answer() {
    let mut t = T::known();
    let (read, _) = t.next_load(40_000).expect("a refresh is out");
    let (call, request) = toggle_of(&t.press("off", true)).expect("a resume");
    t.step(Input::Jobs(read, jobs_reply(three())));
    assert_eq!(switch(&t, "off"), Switch::Off);
    assert_eq!(
        t.view().toggling,
        Some(id("off")),
        "still waiting for the serve"
    );
    t.step(Input::Toggled(call, request, toggled_to("off", "active")));
    assert_eq!(switch(&t, "off"), Switch::On);
}

#[test]
fn an_answer_from_before_the_state_was_thrown_away_is_not_applied() {
    let mut t = calm();
    let (call, request) = toggle_of(&t.press("on", false)).expect("a pause");
    let store = std::sync::Arc::clone(&t.store);
    let _ = std::thread::spawn(move || {
        let _guard = store.lock();
        panic!("a panic with the state locked");
    })
    .join();
    t.after(1_000);
    assert!(t.page().rows.is_empty() && t.view().toggling.is_none());
    let out = t.step(Input::Toggled(call, request, toggled_to("on", "paused")));
    assert!(
        t.notice().is_none() && !out.changed,
        "it is about a page that is gone"
    );
}
