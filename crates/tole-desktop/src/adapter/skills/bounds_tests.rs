//! The caps on counts: a cut is said in a note, so the answer reads as partial.

use serde_json::{json, Value};
use tole_client::{SkillsState, ToolsState};
use tole_desktop_model::{Badge, SkillsPageState};

use super::*;

fn many_skills(n: usize) -> Vec<Value> {
    (0..n)
        .map(|i| {
            json!({"name": format!("s{i}"), "state": "approved", "status": "approved",
                   "hash": null, "approved_hash": null, "approved_at": null,
                   "source": null, "source_text": "", "history": []})
        })
        .collect()
}

#[test]
fn too_many_rows_are_cut_and_the_cut_makes_the_answer_partial() {
    let state: SkillsState = serde_json::from_value(json!({
        "skills": many_skills(MAX_ROWS + 7), "drafts": [], "archived": [], "notes": []
    }))
    .expect("skills");
    let (skills, _, _, notes) = skills_input(&state);
    assert_eq!(skills.len(), MAX_ROWS);
    assert_eq!(skills[0].name, "s0", "the first rows are kept, in order");
    assert_eq!(notes, ["7 more skills not shown"]);
    let mut page = SkillsPageState::default();
    apply_skills(&mut page, &Ok(state));
    assert_eq!(page.skills.approved.len(), MAX_ROWS);
    assert!(page.skills.is_partial());
    assert_eq!(
        page.skills_waiting(),
        Badge::Unknown,
        "never 'nothing to review'"
    );
    assert!(!page.skills_inbox_is_empty());
}

#[test]
fn drafts_archive_notes_and_warnings_are_cut_with_a_note_each() {
    let draft = |i: usize| {
        json!({"id": format!("d{i}"), "name": "n", "description": "", "created_at": "t",
               "hash": "h", "tainted": false, "nearest": null,
               "warnings": (0..MAX_WARNINGS + 3).map(|w| format!("w{w}")).collect::<Vec<_>>()})
    };
    let archived =
        |i: usize| json!({"skill": format!("a{i}"), "entry": 1, "at": "t", "reason": "r"});
    let state: SkillsState = serde_json::from_value(json!({
        "skills": [],
        "drafts": (0..MAX_ROWS + 2).map(draft).collect::<Vec<_>>(),
        "archived": (0..MAX_ROWS + 1).map(archived).collect::<Vec<_>>(),
        "notes": (0..MAX_NOTES + 4).map(|i| format!("note {i}")).collect::<Vec<_>>()
    }))
    .expect("skills");
    let (_, drafts, archived, notes) = skills_input(&state);
    assert_eq!((drafts.len(), archived.len()), (MAX_ROWS, MAX_ROWS));
    assert!(drafts.iter().all(|d| d.warnings.len() == MAX_WARNINGS));
    assert_eq!(
        notes.len(),
        MAX_NOTES + 4,
        "kept notes and a closing note per cut"
    );
    assert_eq!(notes[MAX_NOTES - 1], format!("note {}", MAX_NOTES - 1));
    let tail = &notes[MAX_NOTES..];
    assert_eq!(tail[0], "4 more notes not shown");
    assert_eq!(tail[1], "2 more drafts not shown");
    assert_eq!(tail[2], "1 more archived skills not shown");
    assert_eq!(
        tail[3],
        format!("{} more draft warnings not shown", 3 * MAX_ROWS)
    );
}

#[test]
fn tools_and_their_secret_names_are_cut_with_a_note() {
    let server = |i: usize| {
        json!({"trust": "trusted", "name": format!("t{i}"), "state": "approved", "tier": "user",
               "source": null, "version": null, "approved_at": null, "sandbox": "none",
               "manifest_hash": null, "lock": "-",
               "secrets": (0..MAX_SECRETS + 2).map(|s| format!("S{s}")).collect::<Vec<_>>()})
    };
    let state: ToolsState = serde_json::from_value(json!({
        "servers": (0..MAX_ROWS + 5).map(server).collect::<Vec<_>>(),
        "notes": [], "probed": false
    }))
    .expect("tools");
    let (tools, notes, _) = tools_input(&state);
    assert_eq!(tools.len(), MAX_ROWS);
    assert!(tools.iter().all(|t| t.secrets.len() == MAX_SECRETS));
    assert_eq!(
        notes,
        [
            "5 more tools not shown".to_owned(),
            format!("{} more tool secret names not shown", 2 * MAX_ROWS)
        ]
    );
    let mut page = SkillsPageState::default();
    apply_tools(&mut page, &Ok(state));
    assert_eq!(page.tools_needing_attention(), Badge::Unknown);
    assert!(!page.no_connected_tools());
}

#[test]
fn an_answer_within_the_caps_gets_no_note() {
    let state: SkillsState = serde_json::from_value(json!({
        "skills": many_skills(MAX_ROWS), "drafts": [], "archived": [], "notes": []
    }))
    .expect("skills");
    assert!(skills_input(&state).3.is_empty());
}
