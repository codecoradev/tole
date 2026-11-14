//! #592 end-to-end with the REAL binary: `tole skills drafts` lists, shows
//! and rejects a draft that `skill_propose` stored, and a draft is never
//! visible to the skill loaders.
//!
//! Hermetic: `CODECORA_HOME` and `HOME` point at a temp dir on the child.

use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use tole_core::secrets::SecretSet;
use tole_core::skill_draft::{gate, Dedup, DraftStore, HostContext, Proposal, Trigger};

static COUNTER: AtomicU64 = AtomicU64::new(0);

fn tmpdir() -> PathBuf {
    let n = COUNTER.fetch_add(1, Ordering::SeqCst);
    let d = std::env::temp_dir().join(format!("tole-drafts-e2e-{}-{n}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d.canonicalize().unwrap()
}

fn tole(home: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_tole"))
        .args(args)
        .current_dir(home)
        .env("TOLE_NO_UPDATE_CHECK", "1")
        .env("CODECORA_HOME", home)
        .env("HOME", home)
        .stdin(Stdio::null())
        .output()
        .unwrap()
}

fn out(o: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&o.stdout),
        String::from_utf8_lossy(&o.stderr)
    )
}

const SKILL: &str = "---\nname: ship-release\ndescription: How to cut a release\n---\n\
                     ## Steps\n1. Bump the version.\n";

fn seed(home: &Path) -> String {
    let store = DraftStore::new(home.join("tole").join("skill-drafts"));
    let ctx = HostContext {
        session: Some("s-1".into()),
        model: Some("m".into()),
        taint: vec!["web_fetch".into()],
        dedup: Dedup::from_nearest(None),
        now: 1_700_000_000,
    };
    let p = Proposal {
        name: "ship-release".into(),
        skill_md: SKILL.into(),
        trigger: Trigger::OwnerCorrection,
        sources: Vec::new(),
    };
    let (d, md) = gate(&p, &ctx, &SecretSet::default()).unwrap();
    store.create(&d, &md).unwrap();
    d.id
}

#[test]
fn an_empty_store_says_so() {
    let home = tmpdir();
    let o = tole(&home, &["skills", "drafts"]);
    assert!(o.status.success(), "{}", out(&o));
    assert!(out(&o).contains("no pending drafts"), "{}", out(&o));
}

#[test]
fn a_draft_is_listed_shown_rejected_and_never_loadable() {
    let home = tmpdir();
    let id = seed(&home);

    let o = tole(&home, &["skills", "drafts", "list"]);
    assert!(o.status.success(), "{}", out(&o));
    assert!(
        out(&o).contains(&id) && out(&o).contains("TAINTED"),
        "{}",
        out(&o)
    );

    let o = tole(&home, &["skills", "drafts", "show", &id]);
    let shown = out(&o);
    assert!(o.status.success(), "{shown}");
    assert!(shown.contains("trigger: OwnerCorrection") && shown.contains("web_fetch"));
    assert!(shown.contains("Bump the version."), "{shown}");

    // The loaders know nothing of drafts: not listed, not approvable.
    let o = tole(&home, &["skills", "list"]);
    assert!(out(&o).contains("no skills found"), "{}", out(&o));
    let o = tole(
        &home,
        &["skills", "approve", "ship-release", "--hash", "sha256:0"],
    );
    assert!(!o.status.success());

    let o = tole(&home, &["skills", "drafts", "reject", &id]);
    assert!(o.status.success(), "{}", out(&o));
    let o = tole(&home, &["skills", "drafts", "list"]);
    assert!(out(&o).contains("no pending drafts"), "{}", out(&o));
    let o = tole(&home, &["skills", "drafts", "list", "--all"]);
    assert!(out(&o).contains("rejected"), "{}", out(&o));
    let o = tole(&home, &["skills", "drafts", "reject", &id]);
    assert!(
        !o.status.success() && out(&o).contains("already"),
        "{}",
        out(&o)
    );
}

#[test]
fn promote_makes_an_approved_skill_and_revoke_takes_the_approval_back() {
    let home = tmpdir();
    let ws = home.join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    let ws_arg = ws.to_str().unwrap();
    let id = seed(&home);
    let hash = tole_core::skill_index::content_hash(SKILL.as_bytes());

    // The draft is tainted, so promotion needs the explicit accept.
    let o = tole(
        &home,
        &[
            "skills",
            "drafts",
            "promote",
            &id,
            "--hash",
            &hash,
            "--workspace",
            ws_arg,
        ],
    );
    assert!(
        !o.status.success() && out(&o).contains("--accept-taint"),
        "{}",
        out(&o)
    );
    // Without a terminal the reviewed hash is required.
    let o = tole(
        &home,
        &[
            "skills",
            "drafts",
            "promote",
            &id,
            "--accept-taint",
            "--workspace",
            ws_arg,
        ],
    );
    assert!(
        !o.status.success() && out(&o).contains("--hash"),
        "{}",
        out(&o)
    );

    let o = tole(
        &home,
        &[
            "skills",
            "drafts",
            "promote",
            &id,
            "--hash",
            &hash,
            "--accept-taint",
            "--workspace",
            ws_arg,
        ],
    );
    assert!(o.status.success(), "{}", out(&o));
    assert!(out(&o).contains("promoted"), "{}", out(&o));
    assert_eq!(
        std::fs::read_to_string(ws.join("skills/ship-release/SKILL.md")).unwrap(),
        SKILL
    );
    let o = tole(&home, &["skills", "list", "--workspace", ws_arg]);
    assert!(out(&o).contains("ship-release  approved"), "{}", out(&o));

    // The source session turns out poisoned.
    let o = tole(&home, &["skills", "drafts", "revoke", "s-1"]);
    assert!(o.status.success(), "{}", out(&o));
    assert!(out(&o).contains("withdrawn"), "{}", out(&o));
    let o = tole(&home, &["skills", "list", "--workspace", ws_arg]);
    assert!(
        out(&o).contains("ship-release  not approved"),
        "{}",
        out(&o)
    );
    assert!(ws.join("skills/ship-release/SKILL.md").exists());
    let o = tole(&home, &["skills", "drafts", "list", "--all"]);
    assert!(out(&o).contains("revoked"), "{}", out(&o));
}
