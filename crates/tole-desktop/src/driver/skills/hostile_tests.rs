//! A hostile or broken serve: huge strings, more rows than a page can hold,
//! control and direction-changing characters, a failure that never ends. What
//! reaches the view is bounded, clean, and never a number the server did not
//! stand behind.

use serde_json::{json, Value};
use tole_client::Error;
use tole_desktop_model::{Badge, Load};

use super::engine::{Input, Reply, Route};
use super::fixtures::{
    draft, load_of, skill, skills_err, skills_from, status, tool, tools_from, T,
};
use super::rig::Rig;
use super::view::SkillsPageView;
use crate::adapter::skills::{MAX_NOTES, MAX_ROWS, MAX_SECRETS, MAX_WARNINGS};
use crate::driver::fake::FakeApi;

/// 1 MB of text with a newline, an escape and a right-to-left override inside.
fn bomb() -> String {
    format!(
        "{}\n\u{1b}[31m\u{202e}\u{200b}{}",
        "x".repeat(500_000),
        "y".repeat(500_000)
    )
}

fn clean(s: &str) -> bool {
    !s.chars().any(|c| {
        c.is_control()
            || ('\u{200B}'..='\u{200F}').contains(&c)
            || ('\u{202A}'..='\u{202E}').contains(&c)
    })
}

fn short(s: &str, max: usize) -> bool {
    s.chars().count() <= max && clean(s)
}

/// An answer in which every string the server controls is `bomb`.
fn hostile_skills() -> tole_client::SkillsState {
    let b = bomb();
    let mut approved = skill(&b, "approved", "approved");
    approved["source_text"] = json!(b);
    approved["approved_at"] = json!(b);
    approved["hash"] = json!(b);
    let mut d = draft("d1");
    d["name"] = json!(b);
    d["description"] = json!(b);
    d["id"] = json!(b);
    d["created_at"] = json!(b);
    d["hash"] = json!(b);
    d["nearest"] = json!(b);
    let w = format!("{}\n\u{202e}", "w".repeat(10_000));
    d["warnings"] = json!((0..MAX_WARNINGS * 4).map(|_| w.clone()).collect::<Vec<_>>());
    let mut s = skills_from(vec![approved], vec![d], &[]);
    s.archived = serde_json::from_value(json!([
        {"skill": b, "entry": 1, "at": b, "reason": b}
    ]))
    .unwrap();
    s.notes = (0..MAX_NOTES * 4).map(|_| "n".repeat(20_000)).collect();
    s
}

fn assert_bounded(v: &SkillsPageView) {
    let s = &v.page.skills;
    for a in &s.approved {
        assert!(short(&a.name, 80));
        assert!(a.source.as_deref().is_none_or(|t| short(t, 300)));
        assert!(a.approved_at.as_deref().is_none_or(|t| short(t, 40)));
    }
    for r in &s.review {
        assert!(short(&r.name, 80));
        assert!(r.description.as_deref().is_none_or(|t| short(t, 300)));
        assert!(r.draft_id.as_deref().is_none_or(|t| short(t, 80)));
        assert!(r.nearest.as_deref().is_none_or(|t| short(t, 80)));
        assert!(r.warnings.len() <= MAX_WARNINGS);
        assert!(r.warnings.iter().all(|w| short(w, 300)));
    }
    for a in &s.archived {
        assert!(short(&a.name, 80) && short(&a.at, 40) && short(&a.reason, 300));
    }
    assert!(s.notes.len() <= MAX_NOTES + 2, "{} notes", s.notes.len());
    assert!(s.notes.iter().all(|n| short(n, 200)));
}

#[test]
fn huge_hostile_strings_reach_the_view_bounded_and_clean_and_the_badge_is_unknown() {
    let mut t = T::new();
    let out = t.start();
    t.step(Input::Loaded(
        load_of(&out, Route::Skills).unwrap(),
        Reply::Skills(Ok(hostile_skills())),
    ));
    let v = t.view();
    assert_eq!(v.page.skills_load, Load::Ready);
    assert_bounded(&v);
    assert_eq!(
        v.skills_waiting,
        Badge::Unknown,
        "it said it was cut and partial"
    );
    assert!(!v.page.skills_inbox_is_empty());
    // The 200 warnings per draft over the cap are said, not silent.
    assert!(v
        .page
        .skills
        .notes
        .iter()
        .any(|n| n.contains("draft warnings not shown")));
}

#[test]
fn more_rows_than_a_page_holds_are_cut_and_said_never_nothing_to_review() {
    let mut t = T::new();
    let out = t.start();
    let rows = (0..MAX_ROWS + 500)
        .map(|i| skill(&format!("s{i}"), "approved", "approved"))
        .collect();
    t.step(Input::Loaded(
        load_of(&out, Route::Skills).unwrap(),
        Reply::Skills(Ok(skills_from(rows, vec![], &[]))),
    ));
    let mut servers: Vec<Value> = (0..MAX_ROWS + 200)
        .map(|i| tool(&format!("t{i}"), "trusted", "approved"))
        .collect();
    servers[0]["secrets"] = json!((0..MAX_SECRETS * 10)
        .map(|i| format!("S{i}"))
        .collect::<Vec<_>>());
    t.step(Input::Loaded(
        load_of(&out, Route::Tools).unwrap(),
        Reply::Tools(Ok(tools_from(servers, &[]))),
    ));
    let v = t.view();
    assert_eq!(v.page.skills.approved.len(), MAX_ROWS);
    assert!(v
        .page
        .skills
        .notes
        .contains(&"500 more skills not shown".to_owned()));
    assert_eq!(v.skills_waiting, Badge::Unknown);
    assert!(!v.page.skills_inbox_is_empty());
    assert_eq!(v.page.tools.len(), MAX_ROWS);
    assert_eq!(v.page.tools[0].secrets.len(), MAX_SECRETS);
    assert_eq!(v.tools_needing_attention, Badge::Unknown);
    assert!(!v.page.no_connected_tools());
}

#[test]
fn a_hostile_error_text_is_one_clean_short_line() {
    let mut t = T::new();
    let out = t.start();
    let message = format!("{}\n\u{1b}[2J\u{202e}gnorw", "boom ".repeat(400_000));
    t.step(Input::Loaded(
        load_of(&out, Route::Skills).unwrap(),
        Reply::Skills(Err(status(500, &message))),
    ));
    let v = t.view();
    let Load::Failed(text) = &v.page.skills_load else {
        panic!("a failed tab")
    };
    assert!(short(text, 200), "{} chars", text.chars().count());
    // A failure that is not a status is cleaned the same way.
    t.step(Input::Loaded(
        load_of(&out, Route::Tools).unwrap(),
        Reply::Tools(Err(Error::Io(std::io::Error::other(
            "a\nb\u{202e}c".repeat(10_000),
        )))),
    ));
    let Load::Failed(text) = &t.view().page.tools_load else {
        panic!("a failed tab")
    };
    assert!(short(text, 200));
}

#[test]
fn a_tool_that_claims_more_than_its_state_says_is_not_trusted() {
    let mut t = T::new();
    let out = t.start();
    let servers = vec![
        tool("liar", "trusted", "unapproved"),
        tool("wat", "\u{202e}trusted\n", "approved"),
        tool("fine", "trusted", "approved"),
    ];
    t.step(Input::Loaded(
        load_of(&out, Route::Tools).unwrap(),
        Reply::Tools(Ok(tools_from(servers, &[]))),
    ));
    assert_eq!(t.view().tools_needing_attention, Badge::Count(2));
}

#[test]
fn a_failure_that_never_ends_never_waits_more_than_30_seconds() {
    let mut t = T::new();
    let out = t.start();
    let mut id = load_of(&out, Route::Skills).unwrap();
    for _ in 0..40 {
        t.step(Input::Loaded(id, skills_err()));
        id = t.next_load(Route::Skills, 40_000).expect("a retry").0;
    }
    for _ in 0..20 {
        t.step(Input::Loaded(id, skills_err()));
        assert!(t.next_load(Route::Skills, 29_750).is_none());
        id = t.next_load(Route::Skills, 250).expect("at 30 s").0;
    }
    assert_eq!(t.view().skills_feed.failures, 60);
    assert_eq!(t.view().skills_waiting, Badge::Unknown);
}

#[test]
fn a_hostile_answer_through_the_threads_is_bounded_too() {
    let api = FakeApi::new();
    api.reply_skills(Ok(hostile_skills()));
    api.reply_tools(Err(status(429, &bomb())));
    let rig = Rig::start(api);
    let v = rig.until("both answered", |v| {
        v.page.skills_load == Load::Ready && matches!(v.page.tools_load, Load::Failed(_))
    });
    assert_bounded(&v);
    let Load::Failed(text) = &v.page.tools_load else {
        unreachable!()
    };
    assert!(short(text, 200));
    assert_eq!(v.skills_waiting, Badge::Unknown);
    assert_eq!(v.tools_needing_attention, Badge::Unknown);
}
