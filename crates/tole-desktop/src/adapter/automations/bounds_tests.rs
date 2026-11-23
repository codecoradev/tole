//! The caps on counts and sizes, and what a hostile answer cannot do.

use serde_json::json;
use tole_desktop_model::{AutomationRow, NextRun, RunEntry, RunOutcome, Switch, ToggleResult};

use super::fixtures::*;
use super::text::{cut, OUTPUT_MAX, TEXT_MAX};
use super::*;

fn many(n: usize) -> Vec<serde_json::Value> {
    (0..n)
        .map(|i| job_json(&format!("j{i}"), "cli", "active", false))
        .collect()
}

#[test]
fn too_many_jobs_are_cut_and_the_count_of_what_was_left_out_is_kept() {
    let got = jobs(&jobs_answer(many(MAX_JOBS + 7)));
    assert_eq!(got.rows.len(), MAX_JOBS);
    assert_eq!(got.left_out, 7);
    assert_eq!(got.rows[0].id, "j0", "the first rows are kept, in order");
    let within = jobs(&jobs_answer(many(MAX_JOBS)));
    assert_eq!((within.rows.len(), within.left_out), (MAX_JOBS, 0));
}

#[test]
fn five_thousand_jobs_are_one_cap_and_one_count() {
    let got = jobs(&jobs_answer(many(5_000)));
    assert_eq!((got.rows.len(), got.left_out), (MAX_JOBS, 4_500));
}

#[test]
fn a_second_job_with_the_same_id_is_left_out_not_shown_as_another_row() {
    let list = jobs_answer(vec![
        job_json("a", "cli", "active", false),
        job_json("b", "cli", "paused", false),
        job_json("a", "agent", "paused", true),
    ]);
    let got = jobs(&list);
    let ids: Vec<_> = got.rows.iter().map(|r| r.id.as_str()).collect();
    assert_eq!((ids, got.left_out), (vec!["a", "b"], 1));
    assert_eq!(
        got.rows[0].created_by, "cli",
        "the first one is the one kept"
    );
}

#[test]
fn too_many_runs_are_cut_with_a_lower_bound_of_the_rest() {
    let list = runs_answer(
        (0..RUNS_LIMIT + 4)
            .map(|i| run_json(&format!("f{i}"), "j", Some("ok"), None))
            .collect(),
    );
    let got = runs("j", &list, None);
    assert_eq!((got.rows.len(), got.more_at_least), (MAX_RUNS, 5));
    assert_eq!(
        got.rows[0].fire_id, "f0",
        "newest first, as the server sent them"
    );
    let full = runs_answer(
        (0..RUNS_LIMIT)
            .map(|i| run_json(&format!("f{i}"), "j", Some("ok"), None))
            .collect(),
    );
    assert_eq!(
        runs("j", &full, None).more_at_least,
        1,
        "there may be older ones"
    );
}

#[test]
fn a_cut_marks_itself_and_never_slices_a_character() {
    assert_eq!(cut("short", 10), ("short".to_owned(), false));
    assert_eq!(cut("abc", 3), ("abc".to_owned(), false));
    assert_eq!(cut("abcd", 3), ("abc\u{2026}".to_owned(), true));
    let wide = "\u{1F600}".repeat(10);
    let (kept, was_cut) = cut(&wide, 4);
    assert!(was_cut && kept.chars().count() == 5 && kept.ends_with('\u{2026}'));
    assert_eq!(cut(&"\u{e9}".repeat(8), 8), ("\u{e9}".repeat(8), false));
}

#[test]
fn a_megabyte_in_every_text_field_is_bounded_and_the_page_draws_a_short_line() {
    let huge = "x\u{1b}[31m\u{202E}\n".repeat(100_000);
    let mut job = job_json("j", "cli", "active", false);
    for key in [
        "name",
        "timezone",
        "next",
        "last",
        "created_by",
        "state",
        "schedule",
    ] {
        job[key] = json!(huge);
    }
    let list = jobs_answer(vec![job]);
    let raw = raw_job(&list.jobs[0]);
    for text in [
        &raw.name,
        &raw.schedule,
        &raw.timezone,
        &raw.next,
        &raw.created_by,
        &raw.state,
    ] {
        assert_eq!(text.chars().count(), TEXT_MAX + 1);
    }
    assert_eq!(raw.id, "j");
    let row = AutomationRow::new(raw);
    assert!(row.name.chars().count() <= 80);
    for line in [&row.name, &row.timezone, row.schedule.expr()] {
        assert!(!line.contains(['\u{1b}', '\n', '\u{202E}']), "{line:?}");
    }
    assert_eq!(row.schedule.words(), None);
    assert_eq!(
        row.switch,
        Switch::Off,
        "a state that is not `active` is not on"
    );
}

#[test]
fn a_cut_never_makes_a_word_the_model_knows() {
    let padding = " ".repeat(TEXT_MAX + 10);
    let mut job = job_json("j", "cli", "active", false);
    job["state"] = json!(format!("active{padding}x"));
    job["schedule"] = json!(format!("0 2 * * *{padding}x"));
    job["created_by"] = json!(format!("cli{padding}x"));
    let row = AutomationRow::new(raw_job(&jobs_answer(vec![job]).jobs[0]));
    assert_eq!(row.switch, Switch::Off);
    assert_eq!(row.next, NextRun::Paused);
    assert_eq!(row.schedule.words(), None, "not \"Every night at 02:00\"");
    assert_eq!(row.note(), None);
    assert_eq!(row.origin, tole_desktop_model::Origin::Unknown);
}

#[test]
fn the_id_is_never_cut_or_changed() {
    let long = format!("night\u{1b}[0m\n{}", "z".repeat(200));
    let list = jobs_answer(vec![job_json(&long, "cli", "active", false)]);
    assert_eq!(raw_job(&list.jobs[0]).id, long);
    assert_eq!(jobs(&list).rows[0].id, long, "kept, not cleaned");
    let ok = toggle_result(
        &tole_desktop_model::AutomationId::new(&long),
        &Ok(job_state(&long, "paused")),
    );
    assert_eq!(
        ok,
        ToggleResult::Done {
            state: "paused".into()
        }
    );
}

#[test]
fn a_huge_run_output_is_bounded_and_says_it_was_cut() {
    let mut run = run_json("f", "j", Some("ok"), Some("s"));
    run["output"] = json!("line\n".repeat(200_000));
    let list = runs_answer(vec![run, run_json("g", "j", Some("ok"), None)]);
    let got = runs("j", &list, None);
    assert_eq!(got.rows[0].output.chars().count(), OUTPUT_MAX + 1);
    assert!(
        got.rows[0].output_truncated,
        "the page says the output is cut"
    );
    assert!(!got.rows[1].output_truncated);
    let entry = RunEntry::new(got.rows[0].clone());
    assert!(entry.output.chars().count() <= 600 && !entry.output.contains('\n'));
    assert_eq!(entry.outcome, RunOutcome::Passed);
}

#[test]
fn a_huge_refusal_text_is_bounded() {
    let result = toggle_result(
        &tole_desktop_model::AutomationId::new("j"),
        &Err(status_error(500, &"boom ".repeat(100_000))),
    );
    let ToggleResult::Refused { status, message } = result else {
        panic!("a refusal");
    };
    assert_eq!((status, message.chars().count()), (500, TEXT_MAX + 1));
}

/// A job with an id of `n` bytes of `i`s.
fn with_id_of(n: usize) -> serde_json::Value {
    job_json(&"i".repeat(n), "cli", "active", false)
}

#[test]
fn an_id_at_the_cap_is_kept_and_one_byte_over_is_left_out() {
    let got = jobs(&jobs_answer(vec![
        with_id_of(MAX_ID_BYTES),
        with_id_of(MAX_ID_BYTES + 1),
        job_json("short", "cli", "paused", false),
    ]));
    let ids: Vec<usize> = got.rows.iter().map(|r| r.id.len()).collect();
    assert_eq!(ids, [MAX_ID_BYTES, 5]);
    assert_eq!(got.left_out, 1, "counted, as any other job left out");
    // The cap is in bytes, not characters: 128 two-byte letters are 256.
    let wide = job_json(&"\u{e9}".repeat(MAX_ID_BYTES / 2), "cli", "active", false);
    let wider = job_json(
        &"\u{e9}".repeat(MAX_ID_BYTES / 2 + 1),
        "cli",
        "active",
        false,
    );
    let got = jobs(&jobs_answer(vec![wide, wider]));
    assert_eq!((got.rows.len(), got.left_out), (1, 1));
}

#[test]
fn a_job_with_a_megabyte_id_is_left_out_and_the_count_says_so() {
    let list = jobs_answer(vec![
        job_json("a", "cli", "active", false),
        with_id_of(1 << 20),
        job_json("b", "cli", "paused", false),
    ]);
    let got = jobs(&list);
    assert_eq!(got.rows.len(), 2);
    assert_eq!(got.left_out, 1);
    let mut page = tole_desktop_model::AutomationsState::default();
    assert_eq!(
        apply_jobs(&mut page, &Ok(list)),
        JobsApplied::Ready { left_out: 1 }
    );
    assert_eq!(page.rows.len(), 2);
}

#[test]
fn a_thousand_long_ids_do_not_grow_what_is_kept() {
    let mut jobs_json: Vec<_> = (0..1_000).map(|_| with_id_of(1 << 20)).collect();
    // Distinct ones, so only the length can be what leaves them out.
    for (i, j) in jobs_json.iter_mut().enumerate() {
        j["id"] = json!(format!("{i}{}", "i".repeat(MAX_ID_BYTES)));
    }
    jobs_json.extend((0..2 * MAX_JOBS).map(|i| {
        job_json(
            &format!("{i}{}", "k".repeat(MAX_ID_BYTES - 6)),
            "cli",
            "active",
            false,
        )
    }));
    let got = jobs(&jobs_answer(jobs_json));
    let kept: usize = got.rows.iter().map(|r| r.id.len()).sum();
    assert_eq!(got.rows.len(), MAX_JOBS);
    assert!(kept <= MAX_JOBS * MAX_ID_BYTES, "{kept} bytes of ids kept");
    assert_eq!(got.left_out, 1_000 + 2 * MAX_JOBS - MAX_JOBS);
}
