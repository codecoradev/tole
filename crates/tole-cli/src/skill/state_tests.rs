use super::*;
use crate::test_dir::TestDir;
use tole_core::secrets::SecretSet;
use tole_core::skill_draft::{gate, Dedup, HostContext, Proposal, Trigger};

const SKILL: &str = "---\nname: ship-release\ndescription: How to cut a release\n---\n\
                     ## Steps\n1. Bump the version.\n";

fn scratch(tag: &str) -> TestDir {
    TestDir::new(&format!("tole-state-{tag}"))
}

/// A store holding one pending draft.
fn store_with_draft(dir: &Path) -> DraftStore {
    let store = DraftStore::new(dir.join("drafts"));
    let ctx = HostContext {
        session: Some("s-1".into()),
        model: None,
        taint: Vec::new(),
        dedup: Dedup::from_nearest(None),
        now: 1_700_000_000,
    };
    let p = Proposal {
        name: "ship-release".into(),
        skill_md: SKILL.into(),
        trigger: Trigger::DeadEnd,
        sources: Vec::new(),
    };
    let (d, md) = gate(&p, &ctx, &SecretSet::default()).unwrap();
    store.create(&d, &md).unwrap();
    store
}

/// An archive holding one archived skill.
fn archive_with_skill(dir: &Path) -> Archive {
    let pkg = dir.join("old-one");
    std::fs::create_dir_all(&pkg).unwrap();
    std::fs::write(
        pkg.join("SKILL.md"),
        "---\nname: old-one\ndescription: \"old\"\n---\nold",
    )
    .unwrap();
    let a = Archive::new(dir.join("ledger.jsonl"), dir.join("archive"));
    a.archive("old-one", &pkg, "unused", 10).unwrap();
    a
}

#[test]
fn readable_drafts_and_archive_add_no_note() {
    let d = scratch("ok");
    let mut notes = Vec::new();
    let drafts = drafts_json(Some(store_with_draft(&d)), &mut notes);
    let archived = archived_json(Some(archive_with_skill(&d)), &mut notes);
    assert_eq!((drafts.len(), archived.len()), (1, 1));
    assert!(notes.is_empty(), "{notes:?}");
}

#[test]
fn a_store_and_an_archive_not_created_yet_are_empty_without_a_note() {
    let d = scratch("fresh");
    let mut notes = Vec::new();
    let store = DraftStore::new(d.join("never"));
    let archive = Archive::new(d.join("never.jsonl"), d.join("never-archive"));
    assert!(drafts_json(Some(store), &mut notes).is_empty());
    assert!(archived_json(Some(archive), &mut notes).is_empty());
    assert!(notes.is_empty(), "{notes:?}");
}

#[test]
fn a_draft_store_that_cannot_be_listed_is_a_note_not_an_empty_list() {
    let d = scratch("store-file");
    // The store path is a file, so it cannot be listed. Unlike a chmod 000
    // directory this also fails for root.
    let path = d.join("drafts");
    std::fs::write(&path, "not a directory").unwrap();
    let mut notes = Vec::new();
    let drafts = drafts_json(Some(DraftStore::new(&path)), &mut notes);
    assert!(drafts.is_empty());
    assert_eq!(notes.len(), 1, "{notes:?}");
    assert!(notes[0].starts_with("drafts not read: "), "{notes:?}");
}

#[test]
fn a_corrupt_draft_is_a_note_and_the_others_stay() {
    let d = scratch("corrupt");
    let store = store_with_draft(&d);
    let broken = d.join("drafts").join("dbroken");
    std::fs::create_dir_all(&broken).unwrap();
    std::fs::write(broken.join("draft.json"), "{").unwrap();
    let mut notes = Vec::new();
    let drafts = drafts_json(Some(store), &mut notes);
    assert_eq!(drafts.len(), 1);
    assert_eq!(notes.len(), 1, "{notes:?}");
    assert!(
        notes[0].starts_with("drafts not read: 1 unreadable"),
        "{notes:?}"
    );
}

#[test]
fn an_archive_ledger_that_cannot_be_read_is_a_note_not_an_empty_list() {
    let d = scratch("ledger-dir");
    let ledger = d.join("ledger.jsonl");
    std::fs::create_dir_all(&ledger).unwrap();
    let mut notes = Vec::new();
    let archived = archived_json(Some(Archive::new(&ledger, d.join("archive"))), &mut notes);
    assert!(archived.is_empty());
    assert_eq!(notes.len(), 1, "{notes:?}");
    assert!(notes[0].starts_with("archive not read: "), "{notes:?}");
}

#[test]
fn an_unresolvable_data_directory_is_a_note_for_each_part() {
    let mut notes = Vec::new();
    assert!(drafts_json(None, &mut notes).is_empty());
    assert!(archived_json(None, &mut notes).is_empty());
    assert_eq!(
        notes,
        [
            "drafts not read: the tole data directory cannot be resolved",
            "archive not read: the tole data directory cannot be resolved"
        ]
    );
}
