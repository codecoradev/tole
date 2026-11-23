//! A hostile or broken serve: huge text, thousands of jobs, odd ids, repeated
//! ids and answers that name the wrong thing. Whatever it sends, a thread does
//! not panic, the page draws short clean lines, and the switch acts on exactly
//! the job the owner pressed.

use serde_json::json;
use tole_desktop_model::{Load, Notice, Switch, ToggleIntent};

use super::engine::{Input, RunsReply};
use super::fixtures::*;

const MEGABYTE: usize = 1 << 20;

fn clean_line(s: &str, max: usize) -> bool {
    s.chars().count() <= max && !s.contains(['\n', '\r', '\u{1b}', '\u{202E}', '\u{200B}'])
}

#[test]
fn a_megabyte_in_every_text_field_is_one_short_clean_line_and_no_panic() {
    let huge = format!("x\u{1b}[31m\u{202E}\n\u{200B}{}", "y".repeat(MEGABYTE));
    let mut job = job_json("j", "cli", "active", false);
    for key in ["name", "schedule", "timezone", "next", "last"] {
        job[key] = json!(huge);
    }
    let mut t = T::new();
    let call = load_of(&t.start()).expect("first");
    t.step(Input::Jobs(call, jobs_reply(jobs_answer(vec![job]))));
    let v = t.view();
    let row = &v.page.rows[0];
    assert!(clean_line(&row.name, 80) && clean_line(&row.timezone, 64));
    assert!(clean_line(row.schedule.expr(), 256) && row.schedule.words().is_none());
    assert_eq!(v.page.load, Load::Ready);
}

#[test]
fn a_megabyte_of_run_output_and_an_odd_status_are_bounded_and_not_a_pass() {
    let mut t = T::known();
    let mut run = run_json("f", "on", Some("ok\u{1b}[0m"), Some("s"));
    run["output"] = json!("line\n".repeat(MEGABYTE / 4));
    run["fire_id"] = json!("f".repeat(MEGABYTE));
    let (call, _) = runs_call_of(&t.step(Input::Select(Some("on".into())))).expect("a call");
    t.step(Input::Runs(call, runs_reply("on", runs_answer(vec![run]))));
    let v = t.view();
    let entry = &v.page.history[0];
    assert!(clean_line(&entry.output, 600) && entry.output_truncated);
    assert!(clean_line(&entry.fire_id, 80));
    assert!(
        !entry.outcome.is_pass(),
        "a word nobody taught the model is not a pass"
    );
}

#[test]
fn five_thousand_jobs_are_five_hundred_rows_and_a_count_of_the_rest() {
    let many: Vec<_> = (0..5_000)
        .map(|i| job_json(&format!("j{i}"), "cli", "active", false))
        .collect();
    let mut t = T::new();
    let call = load_of(&t.start()).expect("first");
    t.step(Input::Jobs(call, jobs_reply(jobs_answer(many))));
    let v = t.view();
    assert_eq!(
        (v.page.rows.len(), v.left_out, v.counts()),
        (500, 4_500, None)
    );
    // A job past the cap cannot be switched from here: it is not a row.
    let out = t.press("j4000", false);
    assert!(toggle_of(&out).is_none());
    assert_eq!(t.notice(), Some(Notice::Gone));
}

#[test]
fn an_id_with_control_characters_is_sent_back_exactly_as_it_came() {
    let odd = format!("night\u{1b}[0m\n\u{202E}/..?x=1 {}", "z".repeat(100));
    let list = jobs_answer(vec![job_json(&odd, "cli", "active", false)]);
    let mut t = T::new();
    let call = load_of(&t.start()).expect("first");
    t.step(Input::Jobs(call, jobs_reply(list)));
    t.after(2_000);
    let (_, request) = toggle_of(&t.press(&odd, false)).expect("a pause");
    assert_eq!(request.id.as_str(), odd, "never cleaned, never cut");
    assert_eq!(request.intent, ToggleIntent::Pause);
    let shown = t.page().rows[0].id.display();
    assert!(clean_line(&shown, 80), "what is drawn is cleaned");
}

#[test]
fn two_jobs_with_one_id_are_one_row_and_a_press_acts_on_the_first() {
    let list = jobs_answer(vec![
        job_json("a", "cli", "active", false),
        job_json("a", "agent", "paused", true),
    ]);
    let mut t = T::new();
    let call = load_of(&t.start()).expect("first");
    t.step(Input::Jobs(call, jobs_reply(list)));
    let v = t.view();
    assert_eq!((v.page.rows.len(), v.left_out, v.counts()), (1, 1, None));
    t.after(2_000);
    let (_, request) = toggle_of(&t.press("a", false)).expect("a pause of the first");
    assert_eq!(request.intent, ToggleIntent::Pause);
}

#[test]
fn a_refusal_of_a_megabyte_is_one_short_line() {
    let mut t = T::known();
    t.after(5_000);
    let (call, request) = toggle_of(&t.press("on", false)).expect("a pause");
    let words = format!("boom\n{}\u{1b}[31m", "x".repeat(MEGABYTE));
    t.step(Input::Toggled(
        call,
        request,
        Err(status_error(500, &words)),
    ));
    let Some(Notice::Failed(text)) = t.notice() else {
        panic!("a failure notice");
    };
    assert!(clean_line(&text, 200));
    assert_eq!(t.page().row(&id("on")).map(|r| r.switch), Some(Switch::On));
}

#[test]
fn an_error_of_a_megabyte_on_a_read_is_one_short_line() {
    let mut t = T::new();
    let call = load_of(&t.start()).expect("first");
    let words = format!("boom\n\u{202E}{}", "x".repeat(MEGABYTE));
    t.step(Input::Jobs(call, Err(status_error(500, &words))));
    let Load::Failed(text) = t.page().load else {
        panic!("failed");
    };
    assert!(clean_line(&text, 200));
    let (call, _) = t.next_load(5_000).expect("retry");
    t.step(Input::Jobs(call, jobs_reply(three())));
    let (call, _) = runs_call_of(&t.step(Input::Select(Some("on".into())))).expect("history");
    let words = format!("bad\n{}", "y".repeat(MEGABYTE));
    let failed = RunsReply {
        job: id("on"),
        runs: Err(status_error(500, &words)),
        lookup: None,
    };
    t.step(Input::Runs(call, failed));
    let Load::Failed(text) = t.view().history.load else {
        panic!("a failed history");
    };
    assert!(clean_line(&text, 200));
}

#[test]
fn a_flood_of_answers_nobody_asked_for_changes_nothing() {
    let mut t = T::known();
    let stranger = {
        let mut other = T::new();
        load_of(&other.start()).expect("first")
    };
    let before = t.view();
    for _ in 0..10_000 {
        let out = t.step(Input::Jobs(stranger, jobs_reply(three())));
        assert!(!out.changed && out.effects.is_empty());
    }
    assert_eq!(t.view(), before);
}

#[test]
fn a_run_list_that_is_all_of_another_job_shows_nothing_for_this_one() {
    let mut t = T::known();
    let (call, _) = runs_call_of(&t.step(Input::Select(Some("on".into())))).expect("a call");
    let foreign = runs_answer(
        (0..50)
            .map(|i| run_json(&format!("f{i}"), "off", Some("ok"), None))
            .collect(),
    );
    t.step(Input::Runs(call, runs_reply("on", foreign)));
    let v = t.view();
    assert_eq!(v.history.load, Load::Ready);
    assert!(
        v.page.history.is_empty(),
        "no other automation's runs under this one"
    );
}

#[test]
fn a_job_whose_id_is_too_long_to_send_is_left_out_and_cannot_be_switched() {
    let long = "j".repeat(1 << 20);
    let list = jobs_answer(vec![
        job_json("a", "cli", "active", false),
        job_json(&long, "cli", "active", false),
    ]);
    let mut t = T::new();
    let call = load_of(&t.start()).expect("first");
    t.step(Input::Jobs(call, jobs_reply(list)));
    let v = t.view();
    assert_eq!((v.page.rows.len(), v.left_out, v.counts()), (1, 1, None));
    t.after(2_000);
    assert!(toggle_of(&t.press(&long, false)).is_none());
    assert_eq!(t.notice(), Some(Notice::Gone));
}
