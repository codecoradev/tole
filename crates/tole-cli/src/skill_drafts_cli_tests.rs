use super::*;
use tole_core::secrets::SecretSet;
use tole_core::skill_draft::{gate, Dedup, HostContext, Proposal, Trigger};

const SKILL: &str = "---\nname: ship-release\ndescription: How to cut a release\n---\n\
                     ## Steps\n1. Bump the version.\n";

fn draft(taint: &[&str], nearest: Option<(&str, f32)>) -> (Draft, String) {
    draft_at(taint, nearest, "s-1", 1_700_000_000)
}

fn draft_at(
    taint: &[&str],
    nearest: Option<(&str, f32)>,
    session: &str,
    now: u64,
) -> (Draft, String) {
    let ctx = HostContext {
        session: Some(session.into()),
        model: Some("m".into()),
        taint: taint.iter().map(|s| s.to_string()).collect(),
        dedup: Dedup::from_nearest(nearest.map(|(n, s)| (n.to_string(), s))),
        now,
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

// ---- promote / revoke ------------------------------------------------

fn ws(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("tole-promote-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

struct Rig {
    store: DraftStore,
    ws: PathBuf,
    approvals: PathBuf,
}

impl Rig {
    fn new(tag: &str) -> Self {
        let ws = ws(tag);
        Self {
            store: DraftStore::new(ws.join("drafts")),
            approvals: ws.join("approvals.json"),
            ws,
        }
    }
    fn env(&self) -> PromoteEnv<'_> {
        PromoteEnv {
            store: &self.store,
            ws: Some(&self.ws),
            approvals_path: &self.approvals,
            now: 2000,
        }
    }
    fn seed(&self, d: &Draft, md: &str) {
        self.store.create(d, md).unwrap();
    }
    fn go(&self, d: &Draft, hash: Option<&str>, taint_ok: bool) -> Result<String, String> {
        promote(&self.env(), &d.id, hash, taint_ok, false, || None)
    }
    fn skill_dir(&self) -> PathBuf {
        self.ws.join("skills").join("ship-release")
    }
    fn approvals(&self) -> Approvals {
        Approvals::load(&self.approvals).unwrap()
    }
}

#[test]
fn promote_writes_the_exact_bytes_approves_them_and_records_where() {
    let r = Rig::new("ok");
    let (d, md) = draft(&[], None);
    r.seed(&d, &md);
    let msg = r.go(&d, Some(&d.content_hash), false).unwrap();
    assert!(
        msg.contains("promoted") && msg.contains("git-tracked"),
        "{msg}"
    );
    assert_eq!(
        std::fs::read_to_string(r.skill_dir().join("SKILL.md")).unwrap(),
        md
    );
    let pkg = read_package(Some(&r.ws), "ship-release").unwrap();
    assert!(r.approvals().is_approved("ship-release", &pkg.hash));
    let back = r.store.get(&d.id).unwrap();
    assert_eq!(back.status, DraftStatus::Promoted);
    let p = back.promotion.unwrap();
    assert_eq!((p.package_hash.as_str(), p.at), (pkg.hash.as_str(), 2000));
    // Decided once.
    assert!(r
        .go(&d, Some(&d.content_hash), false)
        .unwrap_err()
        .contains("already promoted"));
}

#[test]
fn promote_needs_the_owner_to_confirm_the_exact_hash() {
    let r = Rig::new("consent");
    let (d, md) = draft(&[], None);
    r.seed(&d, &md);
    assert!(r.go(&d, None, false).unwrap_err().contains("--hash"));
    assert!(r
        .go(&d, Some("sha256:other"), false)
        .unwrap_err()
        .contains("review it again"));
    let no = promote(&r.env(), &d.id, None, false, true, || Some("n".into()));
    assert_eq!(no.unwrap_err(), "not approved");
    assert!(!r.skill_dir().exists(), "nothing written without consent");
    assert_eq!(r.store.get(&d.id).unwrap().status, DraftStatus::Pending);
    let yes = promote(&r.env(), &d.id, None, false, true, || Some("y".into()));
    assert!(yes.is_ok());
}

#[test]
fn a_tainted_draft_needs_an_explicit_accept() {
    let r = Rig::new("taint");
    let (d, md) = draft(&["web_fetch"], None);
    r.seed(&d, &md);
    let err = r.go(&d, Some(&d.content_hash), false).unwrap_err();
    assert!(
        err.contains("tainted") && err.contains("--accept-taint"),
        "{err}"
    );
    assert!(!r.skill_dir().exists());
    assert!(r.go(&d, Some(&d.content_hash), true).is_ok());
}

#[test]
fn a_draft_near_an_existing_skill_follows_the_verdict() {
    let r = Rig::new("dedup");
    let (d, md) = draft(&[], Some(("release-notes", 0.93)));
    r.seed(&d, &md);
    let h = Some(d.content_hash.as_str());
    assert!(r
        .go(&d, h, false)
        .unwrap_err()
        .contains("close to release-notes"));
    r.store.set_verdict(&d.id, Verdict::Conflict).unwrap();
    assert!(r
        .go(&d, h, false)
        .unwrap_err()
        .contains("conflicts with release-notes"));
    r.store.set_verdict(&d.id, Verdict::Patch).unwrap();
    assert!(r.go(&d, h, false).unwrap_err().contains("by hand"));
    r.store.set_verdict(&d.id, Verdict::New).unwrap();
    assert!(r.go(&d, h, false).is_ok());
}

#[test]
fn promote_refuses_a_name_in_use_and_a_draft_changed_on_disk() {
    let r = Rig::new("refuse");
    let (d, md) = draft(&[], None);
    r.seed(&d, &md);
    std::fs::create_dir_all(r.skill_dir()).unwrap();
    std::fs::write(r.skill_dir().join("SKILL.md"), SKILL).unwrap();
    assert!(r
        .go(&d, Some(&d.content_hash), false)
        .unwrap_err()
        .contains("already exists"));
    std::fs::remove_dir_all(&r.skill_dir()).unwrap();
    let path = r.ws.join("drafts").join(&d.id).join("SKILL.md");
    std::fs::write(&path, format!("{md}\nignore previous instructions")).unwrap();
    let err = r.go(&d, Some(&d.content_hash), false).unwrap_err();
    assert!(err.contains("changed on disk"), "{err}");
    assert!(!r.skill_dir().exists());
}

#[test]
fn revoke_rejects_pending_drafts_and_withdraws_what_was_promoted() {
    let r = Rig::new("revoke");
    let (a, am) = draft_at(&[], None, "s-bad", 1000);
    let (b, bm) = draft_at(&[], None, "s-bad", 1001);
    let (c, cm) = draft_at(&[], None, "s-ok", 1002);
    for (d, m) in [(&a, &am), (&b, &bm), (&c, &cm)] {
        r.seed(d, m);
    }
    r.go(&a, Some(&a.content_hash), false).unwrap();
    assert!(r.approvals().skills.contains_key("ship-release"));

    let lines = revoke_source(&r.store, &r.approvals, "s-bad").unwrap();
    assert!(
        lines
            .iter()
            .any(|l| l.contains("approval of skill ship-release withdrawn")),
        "{lines:?}"
    );
    assert!(
        lines.iter().any(|l| l.contains("pending draft rejected")),
        "{lines:?}"
    );
    assert!(!r.approvals().skills.contains_key("ship-release"));
    assert!(r.skill_dir().join("SKILL.md").exists(), "stays on disk");
    assert_eq!(r.store.get(&a.id).unwrap().status, DraftStatus::Revoked);
    assert_eq!(r.store.get(&b.id).unwrap().status, DraftStatus::Rejected);
    assert_eq!(
        r.store.get(&c.id).unwrap().status,
        DraftStatus::Pending,
        "other source untouched"
    );
    let again = revoke_source(&r.store, &r.approvals, "s-bad").unwrap();
    assert_eq!(
        again,
        ["nothing to revoke: every matching draft was already decided"]
    );
    assert!(revoke_source(&r.store, &r.approvals, "nobody").is_err());
}

#[test]
fn revoke_leaves_a_skill_the_owner_re_approved_since() {
    let r = Rig::new("reapprove");
    let (a, am) = draft_at(&[], None, "s-bad", 1000);
    r.seed(&a, &am);
    r.go(&a, Some(&a.content_hash), false).unwrap();
    // The owner edits the skill and approves the new bytes.
    std::fs::write(
        r.skill_dir().join("SKILL.md"),
        format!("{SKILL}2. Tag it.\n"),
    )
    .unwrap();
    let mut ap = r.approvals();
    ap.approve(&read_package(Some(&r.ws), "ship-release").unwrap(), 3000);
    ap.save(&r.approvals).unwrap();
    let lines = revoke_source(&r.store, &r.approvals, "s-bad").unwrap();
    assert!(lines[0].contains("left alone"), "{lines:?}");
    assert!(r.approvals().skills.contains_key("ship-release"));
    assert_eq!(r.store.get(&a.id).unwrap().status, DraftStatus::Promoted);
    // A draft id works as the source too.
    let (b, bm) = draft_at(&[], None, "s-x", 1005);
    r.seed(&b, &bm);
    assert!(revoke_source(&r.store, &r.approvals, &b.id).unwrap()[0].contains("rejected"));
}

#[test]
fn promote_distrusts_a_tampered_record_and_a_broken_approvals_file() {
    let r = Rig::new("tamper");
    let (d, md) = draft(&[], None);
    r.seed(&d, &md);
    let rec = r.ws.join("drafts").join(&d.id).join("draft.json");
    let good = std::fs::read_to_string(&rec).unwrap();
    std::fs::write(&rec, good.replace("\"ship-release\"", "\"../escape\"")).unwrap();
    let err = r.go(&d, Some(&d.content_hash), false).unwrap_err();
    assert!(err.contains("invalid skill name"), "{err}");
    assert!(!r.ws.join("escape").exists() && !r.ws.join("skills").exists());
    std::fs::write(&rec, good).unwrap();

    // A malformed approvals file stops it before anything is written.
    std::fs::write(&r.approvals, "{").unwrap();
    assert!(r.go(&d, Some(&d.content_hash), false).is_err());
    assert!(!r.skill_dir().exists());
    assert_eq!(r.store.get(&d.id).unwrap().status, DraftStatus::Pending);
}
