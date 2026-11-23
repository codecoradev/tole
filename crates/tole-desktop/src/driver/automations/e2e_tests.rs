//! The driver end to end: real threads, the real model, a scripted [`FakeApi`]
//! and a [`ManualClock`], so no socket and no real sleep. Waits are on
//! conditions (a call arrived, the view changed), bounded in real time only so
//! that a broken driver fails a test instead of hanging it.

use std::time::Duration;

use tole_desktop_model::{AutomationId, Load, Notice, Switch};

use super::engine::STALE;
use super::fixtures::*;
use super::rig::{known, Rig};
use super::AutomationsView;
use crate::adapter::automations::RUNS_LIMIT;
use crate::driver::fake::FakeApi;

fn switch_of(v: &AutomationsView, job: &str) -> Option<Switch> {
    v.page.row(&AutomationId::new(job)).map(|r| r.switch)
}

#[test]
fn the_page_is_not_loaded_until_the_answer_arrives() {
    let api = FakeApi::new();
    let gate = api.gate_cron_jobs();
    let rig = Rig::start(api.clone());
    api.wait_cron_jobs(1);
    let v = rig.handle.snapshot();
    assert_eq!(v.page.load, Load::Loading);
    assert!(v.page.rows.is_empty() && v.jobs_feed.loading && v.counts().is_none());
    assert!(rig.handle.is_running());

    gate.send(jobs_reply(three())).unwrap();
    let v = rig.until("known", |v| v.counts() == Some((1, 2)));
    assert!(!v.jobs_feed.loading && v.page.rows.len() == 3);
}

#[test]
fn a_slow_timer_reads_again_and_the_owner_can_ask_for_it() {
    let (api, rig) = known(2);
    rig.advance(Duration::from_secs(29));
    rig.sync();
    assert_eq!(api.cron_jobs_calls(), 1, "not before the interval");
    rig.advance(Duration::from_secs(1));
    api.wait_cron_jobs(2);
    rig.until("answered", |v| !v.jobs_feed.loading);
    rig.handle.refresh().unwrap();
    rig.tick();
    api.wait_cron_jobs(3);
}

#[test]
fn a_failed_first_read_is_retried_after_a_second() {
    let api = FakeApi::new();
    api.reply_cron_jobs(Err(io_error()));
    api.reply_cron_jobs(jobs_reply(three()));
    let rig = Rig::start(api.clone());
    let v = rig.until("failed", |v| matches!(v.page.load, Load::Failed(_)));
    assert_eq!((v.jobs_feed.failures, v.page.rows.len()), (1, 0));
    rig.tick();
    api.wait_cron_jobs(2);
    rig.until("known", |v| v.counts() == Some((1, 2)));
}

#[test]
fn a_press_sends_one_resume_and_the_row_changes_when_the_serve_answers() {
    let (api, rig) = known(0);
    let gate = api.gate_resume();
    rig.handle.set_enabled("off", true).unwrap();
    api.wait_resumes(1);
    let v = rig.until("busy", |v| v.toggling.is_some());
    assert_eq!(
        switch_of(&v, "off"),
        Some(Switch::Off),
        "no optimistic flip"
    );
    // More presses while the call is out are not calls.
    for _ in 0..5 {
        rig.handle.set_enabled("off", true).unwrap();
        rig.handle.set_enabled("on", false).unwrap();
    }
    gate.send(toggled_to("off", "active")).unwrap();
    let v = rig.until("on", |v| {
        v.toggling.is_none() && switch_of(v, "off") == Some(Switch::On)
    });
    assert!(v.page.notice.is_none());
    assert_eq!(
        (api.resumes(), api.pauses()),
        (vec!["off".to_owned()], vec![])
    );

    // The list is read again, and shows what the serve says.
    let mut after = three();
    after.jobs[1].state = "active".into();
    after.jobs[1].next_run = Some(1_700_000_000);
    api.reply_cron_jobs(jobs_reply(after));
    rig.tick();
    api.wait_cron_jobs(2);
    rig.until("both on", |v| v.counts() == Some((2, 1)));
    assert_eq!(api.resumes().len() + api.pauses().len(), 1, "once");
}

#[test]
fn a_pause_is_a_pause() {
    let (api, rig) = known(1);
    api.reply_pause(toggled_to("on", "paused"));
    rig.handle.set_enabled("on", false).unwrap();
    api.wait_pauses(1);
    rig.until("off", |v| switch_of(v, "on") == Some(Switch::Off));
    assert_eq!(api.pauses(), ["on"]);
    assert!(api.resumes().is_empty());
}

#[test]
fn a_refused_resume_is_a_notice_and_nothing_is_retried() {
    let (api, rig) = known(2);
    api.reply_resume(Err(status_error(
        403,
        "not reviewed: not saved by its owner",
    )));
    rig.handle.set_enabled("off", true).unwrap();
    let v = rig.until("a notice", |v| v.page.notice.is_some());
    assert!(matches!(
        v.page.notice,
        Some(Notice::NeedsOwnerReview { .. })
    ));
    assert_eq!(switch_of(&v, "off"), Some(Switch::Off));
    rig.handle.dismiss_notice().unwrap();
    rig.until("gone", |v| v.page.notice.is_none());
    rig.tick_for(5);
    assert_eq!(api.resumes().len(), 1, "never twice for one press");
}

/// The race of the issue: a read of the list is out while the owner flips a
/// switch. Its answer, read before the flip, must not put the old state back.
#[test]
fn a_toggle_racing_a_refresh_is_not_undone_by_the_stale_list() {
    let (api, rig) = known(0);
    let old_read = api.gate_cron_jobs();
    let gate = api.gate_resume();
    rig.advance(Duration::from_secs(30));
    api.wait_cron_jobs(2);
    rig.handle.set_enabled("off", true).unwrap();
    api.wait_resumes(1);
    gate.send(toggled_to("off", "active")).unwrap();
    rig.until("the switch is on", |v| {
        v.toggling.is_none() && switch_of(v, "off") == Some(Switch::On)
    });

    let mut after = three();
    after.jobs[1].state = "active".into();
    after.jobs[1].next_run = Some(1_700_000_000);
    api.reply_cron_jobs(jobs_reply(after));
    old_read.send(jobs_reply(three())).unwrap();
    let v = rig.until("the stale answer was read", |v| !v.jobs_feed.loading);
    assert_eq!(switch_of(&v, "off"), Some(Switch::On), "not undone");
    assert_eq!(
        api.cron_jobs_calls(),
        2,
        "the fresh read waits for the pace"
    );
    rig.tick();
    api.wait_cron_jobs(3);
    let v = rig.until("the fresh read", |v| v.counts() == Some((2, 1)));
    assert_eq!(switch_of(&v, "off"), Some(Switch::On));
}

#[test]
fn the_history_of_an_automation_comes_with_the_link_to_its_pending_approval() {
    let (api, rig) = known(0);
    api.reply_cron_runs(Ok(runs_answer(vec![
        run_json("f2", "on", Some("awaiting_approval"), Some("s1")),
        run_json("f1", "on", Some("ok"), None),
    ])));
    api.reply_list(Ok(approvals_answer(vec![approval_json(
        "apr-1", "s1", "pending",
    )])));
    rig.handle.select("on").unwrap();
    let v = rig.until("history", |v| v.history.load == Load::Ready);
    assert_eq!(v.page.history.len(), 2);
    assert_eq!(v.page.history[0].approval_link(), Some("apr-1"));
    assert!(v.history.links_known);
    assert_eq!(api.cron_runs_calls(), [("on".to_owned(), RUNS_LIMIT)]);
    assert_eq!(api.list_sessions(), [None], "all approvals, once");

    rig.handle.close_history().unwrap();
    let v = rig.until("closed", |v| v.history.load == Load::Idle);
    assert!(v.page.selected.is_none() && v.page.history.is_empty());
}

#[test]
fn a_history_with_nothing_to_link_asks_for_no_approvals() {
    let (api, rig) = known(0);
    api.reply_cron_runs(Ok(runs_of("on", &["ok", "error"])));
    rig.handle.select("on").unwrap();
    rig.until("history", |v| v.history.load == Load::Ready);
    assert!(api.list_sessions().is_empty());
}

#[test]
fn a_failed_history_is_marked_and_the_list_goes_on() {
    let (api, rig) = known(1);
    api.reply_cron_runs(Err(status_error(500, "ledger unreadable")));
    rig.handle.select("on").unwrap();
    let v = rig.until("failed", |v| matches!(v.history.load, Load::Failed(_)));
    assert!(v.page.history.is_empty());
    assert_eq!(v.page.load, Load::Ready, "the list is not the history");
    api.reply_cron_runs(Ok(runs_of("on", &["ok"])));
    rig.tick();
    api.wait_cron_runs(2);
    rig.until("recovered", |v| v.history.load == Load::Ready);
}

#[test]
fn a_suspend_marks_the_rows_until_a_read_that_began_after_it_answers() {
    let (api, rig) = known(0);
    let gate = api.gate_cron_jobs();
    rig.clock.set_wall_ms(T0 + 3_600_000);
    rig.tick();
    let v = rig.until("marked", |v| v.page.load == Load::Failed(STALE.into()));
    assert_eq!(
        (v.page.rows.len(), v.counts()),
        (3, None),
        "rows, not a count"
    );
    api.wait_cron_jobs(2);
    gate.send(jobs_reply(three())).unwrap();
    rig.until("known again", |v| v.counts() == Some((1, 2)));
}

/// More commands than the inbox holds: the ones that do not fit are refused
/// (the caller presses again), none is dropped silently, and the worker goes on.
#[test]
fn a_flood_of_commands_is_refused_not_queued_for_ever_and_the_driver_goes_on() {
    let (api, rig) = known(1);
    let gate = api.gate_cron_jobs();
    rig.advance(Duration::from_secs(30));
    api.wait_cron_jobs(2);
    for _ in 0..2_000 {
        // `on` is already on, so a press that gets through sends nothing.
        let _ = rig.handle.set_enabled("on", true);
        let _ = rig.handle.refresh();
    }
    gate.send(jobs_reply(three())).unwrap();
    rig.until("the driver still answers", |v| !v.jobs_feed.loading);
    assert!(rig.handle.is_running());
    assert!(api.pauses().is_empty() && api.resumes().is_empty());
}
