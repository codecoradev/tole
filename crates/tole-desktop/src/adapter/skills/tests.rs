//! The mapping, from answers built out of the server's own JSON.

use serde_json::{json, Value};
use tole_client::{Error, SkillsState, ToolsState};
use tole_desktop_model::{
    Badge, BlockedReason, Load, ReviewReason, SkillsPageState, ToolChange, ToolTrust,
};

use super::*;

fn skills_answer() -> SkillsState {
    serde_json::from_value(json!({
        "skills": [
            {"name": "report", "state": "approved", "status": "approved",
             "hash": "sha256:aa", "approved_hash": "sha256:aa",
             "approved_at": "2026-11-01", "source": {"git": "x"},
             "source_text": "source git pinned abc", "history": [{"at": "t"}]},
            {"name": "edited", "state": "changed",
             "status": "changed since approval: refused until approved again",
             "hash": "sha256:bb", "approved_hash": "sha256:aa",
             "approved_at": null, "source": null, "source_text": "", "history": []},
            {"name": "new", "state": "not_approved", "status": "not approved",
             "hash": null, "approved_hash": null, "approved_at": null,
             "source": null, "source_text": "", "history": []},
            {"name": "odd", "state": "levitating", "status": "?",
             "hash": null, "approved_hash": null, "approved_at": null,
             "source": null, "source_text": "", "history": []}
        ],
        "drafts": [{"id": "d1", "name": "layout", "description": "How to lay out",
                    "created_at": "2026-11-17", "hash": "sha256:cd", "tainted": true,
                    "warnings": ["mentions a command"], "nearest": "report"}],
        "archived": [{"skill": "old", "entry": 4, "at": "2026-10-01", "reason": "unused"}],
        "notes": []
    }))
    .expect("skills")
}

fn tools_answer(notes: Value) -> ToolsState {
    serde_json::from_value(json!({
        "servers": [
            {"trust": "trusted", "name": "cora", "state": "approved", "tier": "user",
             "source": "npm:cora", "version": "1.2", "approved_at": "2026-11-02",
             "secrets": ["CORA_KEY"], "sandbox": "strict", "manifest_hash": "h", "lock": "locked"},
            {"trust": "changed", "name": "uteke", "state": "changed (plugin manifest)",
             "tier": "project", "source": null, "version": null, "approved_at": null,
             "secrets": [], "sandbox": "none", "manifest_hash": null, "lock": "-"},
            {"trust": "wat", "name": "mystery", "state": "approved", "tier": "user",
             "source": null, "version": null, "approved_at": null,
             "secrets": [], "sandbox": "none", "manifest_hash": null, "lock": "-"}
        ],
        "notes": notes, "probed": false
    }))
    .expect("tools")
}

#[test]
fn skills_map_field_for_field() {
    let s = skills_answer();
    let r = raw_skill(&s.skills[0]);
    assert_eq!(
        (r.name.as_str(), r.state.as_str(), r.status.as_str()),
        ("report", "approved", "approved")
    );
    assert_eq!(r.source_text, "source git pinned abc");
    assert_eq!(r.approved_at.as_deref(), Some("2026-11-01"));
    assert_eq!(r.hash.as_deref(), Some("sha256:aa"));
    let d = raw_draft(&s.drafts[0]);
    assert_eq!(
        (d.id.as_str(), d.name.as_str(), d.hash.as_str()),
        ("d1", "layout", "sha256:cd")
    );
    assert!(d.tainted);
    assert_eq!(d.warnings, ["mentions a command"]);
    assert_eq!(d.nearest.as_deref(), Some("report"));
    let a = raw_archived(&s.archived[0]);
    assert_eq!(
        (a.skill.as_str(), a.at.as_str(), a.reason.as_str()),
        ("old", "2026-10-01", "unused")
    );
}

#[test]
fn a_loaded_page_groups_by_state_and_unknown_states_are_blocked() {
    let mut page = SkillsPageState::default();
    apply_skills(&mut page, &Ok(skills_answer()));
    assert_eq!(page.skills_load, Load::Ready);
    assert_eq!(page.skills.approved.len(), 1);
    // The draft first, then the changed and the never-approved skills.
    let reasons: Vec<_> = page.skills.review.iter().map(|r| r.reason).collect();
    assert_eq!(
        reasons,
        [
            ReviewReason::SuggestedByTole,
            ReviewReason::ChangedSinceApproval,
            ReviewReason::NotApprovedYet
        ]
    );
    assert!(matches!(
        page.skills.blocked[0].reason,
        BlockedReason::Unknown(_)
    ));
    assert_eq!(page.skills.archived.len(), 1);
    assert_eq!(page.skills_waiting(), Badge::Count(3));
}

#[test]
fn a_note_makes_the_answer_partial_and_the_badge_unknown() {
    let mut state = skills_answer();
    state.notes = vec![
        "skills not read: /home/x/.tole/skills/bad".into(),
        String::new(),
    ];
    assert_eq!(
        skills_input(&state).3,
        state.notes,
        "notes are passed untouched"
    );
    let mut page = SkillsPageState::default();
    apply_skills(&mut page, &Ok(state));
    assert!(page.skills.is_partial());
    assert_eq!(page.skills_waiting(), Badge::Unknown);
    assert!(!page.skills_inbox_is_empty());
}

#[test]
fn an_empty_complete_answer_is_a_real_zero() {
    let empty: SkillsState =
        serde_json::from_value(json!({"skills": [], "drafts": [], "archived": [], "notes": []}))
            .expect("empty");
    let mut page = SkillsPageState::default();
    apply_skills(&mut page, &Ok(empty));
    assert!(page.skills_inbox_is_empty());
}

#[test]
fn tools_map_and_the_models_trust_rules_apply() {
    let t = tools_answer(json!([]));
    let r = raw_tool(&t.servers[0]);
    assert_eq!(
        (r.trust.as_str(), r.name.as_str(), r.tier.as_str()),
        ("trusted", "cora", "user")
    );
    assert_eq!(r.source.as_deref(), Some("npm:cora"));
    assert_eq!(r.version.as_deref(), Some("1.2"));
    assert_eq!(r.approved_at.as_deref(), Some("2026-11-02"));
    assert_eq!(
        (r.secrets.as_slice(), r.sandbox.as_str()),
        (&["CORA_KEY".to_owned()][..], "strict")
    );
    let mut page = SkillsPageState::default();
    apply_tools(&mut page, &Ok(t));
    assert_eq!(page.tools_load, Load::Ready);
    let trust: Vec<_> = page.tools.iter().map(|t| t.trust).collect();
    assert_eq!(
        trust,
        [
            ToolTrust::Trusted,
            ToolTrust::Changed,
            ToolTrust::NotTrusted
        ]
    );
    assert_eq!(page.tools[1].change, Some(ToolChange::Manifest));
    assert!(!page.tools_probed);
    assert_eq!(page.tools_needing_attention(), Badge::Count(2));
}

#[test]
fn tool_notes_and_probed_reach_the_page_as_they_are() {
    let mut t = tools_answer(json!(["plugins not read: broken.toml"]));
    t.probed = true;
    let (_, notes, probed) = tools_input(&t);
    assert_eq!(notes, ["plugins not read: broken.toml"]);
    assert!(probed);
    let mut page = SkillsPageState::default();
    apply_tools(&mut page, &Ok(t));
    assert_eq!(page.tools_needing_attention(), Badge::Unknown, "partial");
    assert!(!page.no_connected_tools());
}

#[test]
fn a_failed_call_empties_the_page_and_says_why() {
    let mut page = SkillsPageState::default();
    apply_skills(&mut page, &Ok(skills_answer()));
    apply_tools(&mut page, &Ok(tools_answer(json!([]))));
    let mut ids = LoadIds::new();
    ids.skills_started(&mut page).expect("id");
    ids.tools_started(&mut page).expect("id");
    assert_eq!(
        (&page.skills_load, &page.tools_load),
        (&Load::Loading, &Load::Loading)
    );
    let failed = |status| -> Result<(), Error> {
        Err(Error::Status {
            status,
            message: "no route".into(),
            body: Value::Null,
        })
    };
    let Err(e) = failed(404) else { unreachable!() };
    apply_skills(&mut page, &Err(e));
    let Err(e) = failed(500) else { unreachable!() };
    apply_tools(&mut page, &Err(e));
    assert!(matches!(&page.skills_load, Load::Failed(m) if m.contains("no route")));
    assert!(matches!(&page.tools_load, Load::Failed(m) if m.contains("500")));
    assert!(page.tools.is_empty() && page.skills.review.is_empty());
    assert_eq!(page.skills_waiting(), Badge::Unknown);
}

#[test]
fn hostile_text_reaches_the_model_and_the_page_cleans_it() {
    let evil = format!("a\u{202e}b\n\u{1b}[2J{}", "y".repeat(100_000));
    let state: SkillsState = serde_json::from_value(json!({
        "skills": [{"name": evil, "state": "approved", "status": "approved", "hash": null,
                    "approved_hash": null, "approved_at": null, "source": null,
                    "source_text": evil, "history": []}],
        "drafts": [], "archived": [], "notes": [evil]
    }))
    .expect("skills");
    let (skills, _, _, notes) = skills_input(&state);
    assert_eq!(skills[0].name, evil);
    assert_eq!(skills[0].source_text, evil);
    assert_eq!(notes.as_slice(), std::slice::from_ref(&evil));
    let mut page = SkillsPageState::default();
    apply_skills(&mut page, &Ok(state));
    let name = &page.skills.approved[0].name;
    assert!(!name.contains('\u{202e}') && !name.contains('\n') && name.chars().count() <= 80);
    assert!(page.skills.notes[0].chars().count() <= 200);
}

#[test]
fn a_late_reply_never_replaces_a_newer_page() {
    let mut page = SkillsPageState::default();
    let mut ids = LoadIds::new();
    let old = ids.skills_started(&mut page).expect("id");
    let new = ids.skills_started(&mut page).expect("id");
    assert_ne!(old, new);
    // The newer answer says the skill changed since it was approved.
    let mut newer = skills_answer();
    newer.skills[0].state = "changed".into();
    newer.skills[0].status = "changed since approval: refused until approved again".into();
    assert!(ids.apply_skills(&mut page, new, &Ok(newer)));
    assert!(page.skills.approved.is_empty());
    // The older answer (it still shows the skill approved) arrives last.
    assert!(!ids.apply_skills(&mut page, old, &Ok(skills_answer())));
    assert!(
        page.skills.approved.is_empty(),
        "the older answer was ignored"
    );
    assert_eq!(page.skills_load, Load::Ready);
    // A late error does not empty the newer page either.
    let late = Err(Error::Protocol("late".into()));
    assert!(!ids.apply_skills(&mut page, old, &late));
    assert_eq!(page.skills_load, Load::Ready);
}

#[test]
fn a_reply_that_is_not_the_latest_leaves_the_page_loading() {
    let mut page = SkillsPageState::default();
    let mut ids = LoadIds::new();
    let old = ids.tools_started(&mut page).expect("id");
    let _new = ids.tools_started(&mut page).expect("id");
    assert!(!ids.apply_tools(&mut page, old, &Ok(tools_answer(json!([])))));
    assert_eq!(page.tools_load, Load::Loading, "the latest call still runs");
    assert!(page.tools.is_empty());
    // The tabs count separately.
    let skills = ids.skills_started(&mut page).expect("id");
    assert!(ids.apply_skills(&mut page, skills, &Ok(skills_answer())));
}
