//! The mapping, from answers built out of the server's own JSON, read the way
//! the page reads them (through the model).

use serde_json::json;
use tole_client::CronJobs;
use tole_desktop_model::{
    AutomationId, AutomationRow, AutomationsState, Load, NextRun, Notice, RowNote, RunEntry,
    RunOutcome, Scheduler, SchedulerHost, Switch, ToggleResult,
};

use super::fixtures::*;
use super::*;

fn id(s: &str) -> AutomationId {
    AutomationId::new(s)
}

#[test]
fn a_job_maps_field_for_field_and_reads_as_the_page_reads_it() {
    let mut json = job_json("night", "cli", "active", false);
    json["last"] = json!("awaiting_approval");
    json["missed"] = json!(2);
    json["blocked"] = json!(1);
    let list = jobs_answer(vec![json]);
    let raw = raw_job(&list.jobs[0]);
    assert_eq!(
        (raw.id.as_str(), raw.name.as_str(), raw.schedule.as_str()),
        ("night", "job night", "0 2 * * *")
    );
    assert_eq!(
        (raw.missed, raw.blocked, raw.next_run),
        (2, 1, Some(1_700_000_000))
    );
    assert_eq!(raw.folder, None, "the serve row has no working folder");
    let row = AutomationRow::new(raw);
    assert_eq!(row.schedule.words(), Some("Every night at 02:00"));
    assert_eq!(row.next, NextRun::At("2026-11-24 02:00 UTC".into()));
    assert_eq!(row.last, Some(RunOutcome::StoppedToAsk));
    assert_eq!(row.switch, Switch::On);
}

#[test]
fn a_job_tole_made_starts_paused_and_says_so() {
    let list = jobs_answer(vec![job_json("t", "agent", "paused", true)]);
    let row = AutomationRow::new(raw_job(&list.jobs[0]));
    assert_eq!(
        (row.switch, row.next.clone()),
        (Switch::Off, NextRun::Paused)
    );
    assert_eq!(row.note(), Some(RowNote::MadeByTolePaused));
    assert!(!row.can_resume_here());
}

#[test]
fn the_scheduler_is_running_only_when_the_server_says_so() {
    let mut list = jobs_answer(vec![]);
    assert_eq!(
        scheduler(&list.scheduler),
        Scheduler::Running(SchedulerHost::Serve)
    );
    list.scheduler.host = Some("tick".into());
    assert_eq!(
        scheduler(&list.scheduler),
        Scheduler::Running(SchedulerHost::Tick)
    );
    list.scheduler.alive = false;
    assert_eq!(scheduler(&list.scheduler), Scheduler::NotRunning);
    let none: CronJobs = serde_json::from_value(json!({
        "jobs": [], "scheduler": {"alive": true, "host": null, "age_seconds": null}
    }))
    .expect("answer");
    assert!(matches!(
        scheduler(&none.scheduler),
        Scheduler::Running(SchedulerHost::Other(_))
    ));
}

#[test]
fn a_run_maps_and_its_status_reads_as_an_outcome() {
    let list = runs_answer(vec![
        run_json("f1", "j", Some("ok"), None),
        run_json("f2", "j", Some("error"), None),
        run_json("f3", "j", None, None),
        run_json("f4", "j", Some("levitating"), None),
    ]);
    let got = runs("j", &list, None);
    assert!(got.links_known && got.more_at_least == 0);
    let outcomes: Vec<_> = got
        .rows
        .into_iter()
        .map(|r| RunEntry::new(r).outcome)
        .collect();
    assert_eq!(
        outcomes[..3],
        [
            RunOutcome::Passed,
            RunOutcome::Failed,
            RunOutcome::NoResultYet
        ]
    );
    assert!(matches!(&outcomes[3], RunOutcome::Unknown(w) if w == "levitating"));
}

#[test]
fn a_run_that_stopped_to_ask_links_to_the_pending_approval_of_its_session() {
    let list = runs_answer(vec![
        run_json("f1", "j", Some("awaiting_approval"), Some("s1")),
        run_json("f2", "j", Some("awaiting_approval"), Some("s2")),
        run_json("f3", "j", Some("awaiting_approval"), None),
        run_json("f4", "j", Some("ok"), Some("s1")),
    ]);
    assert!(needs_approval_lookup("j", &list));
    let pending = approvals_answer(vec![
        approval_json("old", "s1", "approved"),
        approval_json("a-first", "s1", "pending"),
        approval_json("a-second", "s1", "pending"),
        approval_json("other", "s9", "pending"),
    ]);
    let got = runs("j", &list, Some(&pending));
    assert!(got.links_known);
    let link = |i: usize| {
        RunEntry::new(got.rows[i].clone())
            .approval_link()
            .map(str::to_owned)
    };
    assert_eq!(
        link(0).as_deref(),
        Some("a-first"),
        "the first one still waiting"
    );
    assert_eq!(link(1), None, "s2 has nothing waiting any more");
    assert_eq!(link(2), None, "no session, no link");
    assert_eq!(link(3), None, "a pass has no link");
    assert_eq!(
        got.rows[3].approval_id, None,
        "and none is looked up for it"
    );
}

#[test]
fn a_lookup_that_could_not_be_made_is_not_the_same_as_nothing_waiting() {
    let list = runs_answer(vec![run_json(
        "f1",
        "j",
        Some("awaiting_approval"),
        Some("s1"),
    )]);
    let got = runs("j", &list, None);
    assert!(!got.links_known, "a run that needs the list and has none");
    assert_eq!(got.rows[0].approval_id, None);
    let none_needed = runs_answer(vec![run_json("f1", "j", Some("ok"), Some("s1"))]);
    assert!(!needs_approval_lookup("j", &none_needed));
    assert!(runs("j", &none_needed, None).links_known);
    assert!(runs("j", &runs_answer(vec![]), None).rows.is_empty());
}

#[test]
fn runs_of_another_job_are_not_this_history() {
    let list = runs_answer(vec![
        run_json("f1", "other", Some("ok"), None),
        run_json("f2", "j", Some("error"), None),
    ]);
    let got = runs("j", &list, None);
    assert_eq!(got.rows.len(), 1);
    assert_eq!(got.rows[0].fire_id, "f2");
    assert_eq!(got.more_at_least, 0);
    assert!(!needs_approval_lookup("nobody", &list));
}

#[test]
fn a_toggle_answer_is_done_only_for_the_job_that_was_asked() {
    let j = id("night");
    assert_eq!(
        toggle_result(&j, &Ok(job_state("night", "active"))),
        ToggleResult::Done {
            state: "active".into()
        }
    );
    assert_eq!(
        toggle_result(&j, &Ok(job_state("other", "active"))),
        ToggleResult::Unreachable,
        "an answer about another job flips nothing"
    );
    assert_eq!(
        toggle_result(&j, &Err(io_error())),
        ToggleResult::Unreachable
    );
}

#[test]
fn a_refusal_keeps_its_status_and_words_and_the_model_names_the_notice() {
    let j = id("night");
    let refused = |s, m: &str| toggle_result(&j, &Err(status_error(s, m)));
    assert_eq!(
        refused(403, "not reviewed: night"),
        ToggleResult::Refused {
            status: 403,
            message: "not reviewed: night".into()
        }
    );
    assert_eq!(
        refused(500, "  "),
        ToggleResult::Refused {
            status: 500,
            message: "server answered 500".into()
        },
        "a refusal with no words still says something"
    );
    let mut page = AutomationsState::default();
    page.loaded(
        vec![raw_job(
            &jobs_answer(vec![job_json("night", "cli", "paused", false)]).jobs[0],
        )],
        Scheduler::default(),
    );
    let req = page.request_toggle(&j).expect("a request");
    page.finish_toggle(refused(403, "not reviewed: night"));
    assert!(
        matches!(page.notice, Some(Notice::NeedsOwnerReview { .. })),
        "{req:?}"
    );
}

#[test]
fn the_list_answer_replaces_the_rows_and_an_error_marks_them_failed() {
    let mut page = AutomationsState::default();
    let ok = Ok(jobs_answer(vec![
        job_json("a", "cli", "active", false),
        job_json("b", "cli", "paused", false),
    ]));
    assert_eq!(
        apply_jobs(&mut page, &ok),
        JobsApplied::Ready { left_out: 0 }
    );
    assert_eq!((page.load.clone(), page.counts()), (Load::Ready, (1, 1)));
    let failed = apply_jobs(&mut page, &Err(status_error(503, "busy\nnow")));
    assert_eq!(failed, JobsApplied::Failed);
    assert_eq!(page.rows.len(), 2, "the old rows stay, marked failed");
    assert!(matches!(&page.load, Load::Failed(m) if m == "server answered 503: busy now"));
    assert_eq!(page.scheduler, Scheduler::NotRunning);
}

#[test]
fn a_history_answer_applies_only_to_the_selected_automation() {
    let mut page = AutomationsState::default();
    page.loaded(
        jobs(&jobs_answer(vec![job_json("a", "cli", "active", false)])).rows,
        Scheduler::default(),
    );
    let answer = Ok(runs_answer(vec![run_json("f1", "a", Some("ok"), None)]));
    assert_eq!(
        apply_runs(&mut page, &id("a"), &answer, None),
        None,
        "nothing is selected"
    );
    assert!(page.select(&id("a")));
    let got = apply_runs(&mut page, &id("a"), &answer, None);
    assert_eq!(
        got,
        Some(RunsApplied::Ready {
            more_at_least: 0,
            links_known: true
        })
    );
    assert_eq!(page.history.len(), 1);
    let failed = apply_runs(&mut page, &id("a"), &Err(io_error()), None);
    assert!(matches!(failed, Some(RunsApplied::Failed(m)) if m.contains("i/o error")));
    assert_eq!(
        page.history.len(),
        1,
        "the history is not touched by a failure"
    );
    assert_eq!(apply_runs(&mut page, &id("b"), &answer, None), None);
}

#[test]
fn a_failed_approval_lookup_leaves_the_runs_and_says_the_links_are_not_known() {
    let mut page = AutomationsState::default();
    page.loaded(
        jobs(&jobs_answer(vec![job_json("a", "cli", "active", false)])).rows,
        Scheduler::default(),
    );
    page.select(&id("a"));
    let answer = Ok(runs_answer(vec![run_json(
        "f",
        "a",
        Some("awaiting_approval"),
        Some("s"),
    )]));
    let lookup = Err(io_error());
    let got = apply_runs(&mut page, &id("a"), &answer, Some(&lookup));
    assert_eq!(
        got,
        Some(RunsApplied::Ready {
            more_at_least: 0,
            links_known: false
        })
    );
    assert_eq!(page.history[0].approval_link(), None);
}
