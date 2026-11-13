use super::*;
use tole_core::secrets::SecretSet;
use tole_core::skill_draft::{gate, Dedup, HostContext, Proposal, Trigger};

const SKILL: &str = "---\nname: ship-release\ndescription: How to cut a release\n---\n\
                     ## Steps\n1. Bump the version.\n";

fn draft(taint: &[&str], nearest: Option<(&str, f32)>) -> (Draft, String) {
    let ctx = HostContext {
        session: Some("s-1".into()),
        model: Some("m".into()),
        taint: taint.iter().map(|s| s.to_string()).collect(),
        dedup: Dedup::from_nearest(nearest.map(|(n, s)| (n.to_string(), s))),
        now: 1_700_000_000,
    };
    let p = Proposal {
        name: "ship-release".into(),
        skill_md: SKILL.into(),
        trigger: Trigger::DeadEnd,
        sources: Vec::new(),
    };
    gate(&p, &ctx, &SecretSet::default()).unwrap()
}

fn store(tag: &str) -> DraftStore {
    let d = std::env::temp_dir().join(format!("tole-drafts-cli-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    DraftStore::new(d)
}

#[test]
fn control_and_invisible_characters_are_shown_as_escapes() {
    let raw = "ok\tline\n\u{1b}[31mred\u{1b}[0m\r\u{202e}x\u{E0041}\u{00AD}";
    let shown = printable(raw);
    assert!(!shown.contains('\u{1b}') && !shown.contains('\r'));
    assert!(!shown.contains('\u{202e}') && !shown.contains('\u{E0041}'));
    assert!(shown.starts_with("ok\tline\n\\u{001b}[31m"), "{shown}");
    assert!(shown.contains("\\u{000d}") && shown.contains("\\u{202e}"));
    assert_eq!(printable("plain text"), "plain text");
}

#[test]
fn the_list_shows_pending_by_default_and_marks_taint() {
    let s = store("list");
    assert_eq!(format_list(&s.list(), false), "no pending drafts");
    assert_eq!(format_list(&s.list(), true), "no drafts");
    let (d, md) = draft(&["web_fetch"], None);
    s.create(&d, &md).unwrap();
    let line = format_list(&s.list(), false);
    assert!(line.contains(&d.id) && line.contains("TAINTED") && line.contains("pending"));
    s.decide(&d.id, DraftStatus::Rejected).unwrap();
    assert_eq!(format_list(&s.list(), false), "no pending drafts");
    assert!(format_list(&s.list(), true).contains("rejected"));
}

#[test]
fn show_lays_out_provenance_taint_dedup_and_both_texts() {
    let (d, md) = draft(&["mcp_x", "web_fetch"], Some(("release-notes", 0.93)));
    let out = format_show(&d, &md, Some("NEAREST BODY"));
    assert!(out.contains("trigger: DeadEnd") && out.contains("sessions: s-1"));
    assert!(out.contains("TAINTED") && out.contains("mcp_x, web_fetch"));
    assert!(out.contains("nearest skill: release-notes (score 0.93, proposed Patch"));
    let (draft_at, near_at) = (
        out.find("--- proposed SKILL.md ---").unwrap(),
        out.find("--- nearest skill release-notes").unwrap(),
    );
    assert!(
        draft_at < near_at && out.contains("Bump the version.") && out.contains("NEAREST BODY")
    );
    let (d, md) = draft(&[], None);
    let out = format_show(&d, &md, None);
    assert!(out.contains("nearest skill: none found") && !out.contains("TAINTED"));
}

#[test]
fn reject_and_verdict_act_on_a_pending_draft_once() {
    let s = store("act");
    let (d, md) = draft(&[], Some(("release-notes", 0.85)));
    s.create(&d, &md).unwrap();
    run(
        Some(DraftsAction::Verdict {
            id: d.id.clone(),
            verdict: VerdictArg::Conflict,
        }),
        &s,
    )
    .unwrap();
    assert_eq!(s.get(&d.id).unwrap().dedup.verdict, Some(Verdict::Conflict));
    run(Some(DraftsAction::Reject { id: d.id.clone() }), &s).unwrap();
    assert!(run(Some(DraftsAction::Reject { id: d.id.clone() }), &s).is_err());
    assert!(run(
        Some(DraftsAction::Verdict {
            id: d.id.clone(),
            verdict: VerdictArg::New,
        }),
        &s
    )
    .is_err());
    assert!(run(Some(DraftsAction::Reject { id: "nope".into() }), &s).is_err());
    run(None, &s).unwrap();
}
