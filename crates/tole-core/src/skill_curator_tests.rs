use super::*;

const NOW: u64 = 100 * DAY;

fn tmp(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("tole-curator-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn rec(t: u64, skill: Option<&str>, kind: UsageKind) -> UsageRecord {
    UsageRecord {
        t,
        skill: skill.map(String::from),
        kind,
    }
}

fn facts(name: &str, created_days_ago: u64, last_used_days_ago: Option<u64>) -> SkillFacts {
    SkillFacts {
        name: name.into(),
        managed: true,
        pinned: false,
        cron_protected: false,
        created_at: NOW - created_days_ago * DAY,
        usage: Usage {
            last_used: last_used_days_ago.map_or(0, |d| NOW - d * DAY),
            ..Usage::default()
        },
    }
}

fn life(f: &SkillFacts) -> Option<Lifecycle> {
    assess(f, &Thresholds::default(), NOW).map(|(l, _)| l)
}

#[test]
fn the_ledger_folds_use_per_skill_and_keeps_the_baseline_apart() {
    let l = UsageLedger::new(tmp("fold").join(USAGE_FILE));
    for r in [
        rec(10, Some("a"), UsageKind::Recalled),
        rec(20, Some("a"), UsageKind::Loaded),
        rec(30, Some("a"), UsageKind::Success),
        rec(25, Some("a"), UsageKind::Failure),
        rec(5, Some("b"), UsageKind::Recalled),
        rec(40, None, UsageKind::Success),
    ] {
        l.append(&r).unwrap();
    }
    let s = l.summary();
    let a = &s.skills["a"];
    assert_eq!((a.recalled, a.loaded, a.success, a.failure), (1, 1, 1, 1));
    assert_eq!((a.first_seen, a.last_used), (10, 30));
    assert_eq!(a.success_rate(), Some(0.5));
    let b = &s.skills["b"];
    assert_eq!(b.last_used, 0, "a recall alone is not use");
    assert_eq!((s.baseline.success, s.baseline.last_used), (1, 40));
}

#[test]
fn a_malformed_line_is_skipped_and_a_missing_ledger_is_empty() {
    let dir = tmp("bad");
    let path = dir.join(USAGE_FILE);
    assert_eq!(UsageLedger::new(&path).summary(), UsageSummary::default());
    let l = UsageLedger::new(&path);
    l.append(&rec(1, Some("a"), UsageKind::Loaded)).unwrap();
    std::fs::write(
        &path,
        format!(
            "{}garbage\n\n{{\"t\":1}}\n",
            std::fs::read_to_string(&path).unwrap()
        ),
    )
    .unwrap();
    l.append(&rec(2, Some("a"), UsageKind::Loaded)).unwrap();
    assert_eq!(l.summary().skills["a"].loaded, 2);
}

#[test]
fn only_the_tail_of_an_oversized_ledger_is_read() {
    let dir = tmp("big");
    let path = dir.join(USAGE_FILE);
    let old = serde_json::to_string(&rec(1, Some("old"), UsageKind::Loaded)).unwrap();
    let new = serde_json::to_string(&rec(9, Some("new"), UsageKind::Loaded)).unwrap();
    let mut text = String::new();
    while (text.len() as u64) < MAX_LEDGER_BYTES + 1000 {
        text.push_str(&old);
        text.push('\n');
    }
    text.push_str(&new);
    text.push('\n');
    std::fs::write(&path, text).unwrap();
    let s = UsageLedger::new(&path).summary();
    assert_eq!(s.skills["new"].loaded, 1, "the newest line survives");
}

#[test]
fn a_used_skill_goes_active_stale_archived_by_inactivity() {
    assert_eq!(life(&facts("s", 90, Some(3))), Some(Lifecycle::Active));
    assert_eq!(life(&facts("s", 90, Some(13))), Some(Lifecycle::Active));
    assert_eq!(life(&facts("s", 90, Some(14))), Some(Lifecycle::Stale));
    assert_eq!(life(&facts("s", 90, Some(29))), Some(Lifecycle::Stale));
    assert_eq!(life(&facts("s", 90, Some(30))), Some(Lifecycle::Archived));
    // Using it again brings it back: the lifecycle is derived, not stored.
    assert_eq!(life(&facts("s", 90, Some(0))), Some(Lifecycle::Active));
}

#[test]
fn a_skill_never_used_has_a_grace_period_then_follows_its_age() {
    assert_eq!(life(&facts("s", 5, None)), Some(Lifecycle::Active));
    assert_eq!(life(&facts("s", 29, None)), Some(Lifecycle::Active));
    assert_eq!(life(&facts("s", 30, None)), Some(Lifecycle::Archived));
    let t = Thresholds {
        grace_days: 10,
        ..Thresholds::default()
    };
    assert_eq!(
        assess(&facts("s", 15, None), &t, NOW).map(|a| a.0),
        Some(Lifecycle::Stale)
    );
}

#[test]
fn pinned_cron_named_and_unmanaged_skills_are_never_touched() {
    let mut pinned = facts("s", 90, Some(60));
    pinned.pinned = true;
    let mut cron = facts("s", 90, Some(60));
    cron.cron_protected = true;
    let mut hand = facts("s", 90, Some(60));
    hand.managed = false;
    for f in [&pinned, &cron, &hand] {
        assert_eq!(assess(f, &Thresholds::default(), NOW), None);
    }
    let plan = plan(
        &[hand, facts("b", 90, Some(40)), facts("a", 90, Some(1))],
        &Thresholds::default(),
        NOW,
    );
    assert_eq!(
        plan.iter()
            .map(|a| (a.name.as_str(), a.lifecycle))
            .collect::<Vec<_>>(),
        [("a", Lifecycle::Active), ("b", Lifecycle::Archived)]
    );
    assert!(plan[1].reason.contains("40 days"), "{}", plan[1].reason);
}

#[test]
fn thresholds_must_leave_room_for_the_stale_step() {
    assert!(Thresholds::default().check().is_ok());
    for (stale, archive) in [(0, 30), (30, 30), (30, 14)] {
        let t = Thresholds {
            stale_days: stale,
            archive_days: archive,
            grace_days: 0,
        };
        assert!(t.check().is_err(), "{stale}/{archive}");
    }
}

fn outcomes(success: u64, failure: u64) -> Usage {
    Usage {
        success,
        failure,
        ..Usage::default()
    }
}

#[test]
fn a_skill_doing_worse_than_runs_without_one_is_proposed() {
    let mut weak = facts("weak", 90, Some(1));
    weak.usage = outcomes(2, 8);
    let mut fine = facts("fine", 90, Some(1));
    fine.usage = outcomes(8, 2);
    let mut few = facts("few", 90, Some(1));
    few.usage = outcomes(0, 4);
    let mut pinned = facts("pinned", 90, Some(1));
    pinned.usage = outcomes(0, 9);
    pinned.pinned = true;
    let mut hand = facts("hand", 90, Some(1));
    hand.usage = outcomes(0, 9);
    hand.managed = false;
    let all = [weak, fine, few, pinned, hand];

    let found = underperforming(&all, &outcomes(8, 2));
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].name, "weak");
    assert_eq!(
        (found[0].rate, found[0].baseline, found[0].outcomes),
        (0.2, 0.8, 10)
    );
    // Too little baseline data: no comparison.
    assert!(underperforming(&all, &outcomes(3, 1)).is_empty());
    assert!(underperforming(&all, &Usage::default()).is_empty());
}

fn skill_pkg(root: &Path, name: &str) -> PathBuf {
    let dir = root.join("skills").join(name);
    std::fs::create_dir_all(dir.join("references")).unwrap();
    std::fs::write(
        dir.join("SKILL.md"),
        format!("---\nname: {name}\ndescription: d\n---\nbody\n"),
    )
    .unwrap();
    std::fs::write(dir.join("references/a.md"), "ref").unwrap();
    dir
}

#[test]
fn archiving_moves_the_whole_package_and_restoring_puts_it_back() {
    let root = tmp("move");
    let dir = skill_pkg(&root, "demo");
    let a = Archive::new(root.join(MUTATION_FILE), root.join(ARCHIVE_DIR));
    let m = a.archive("demo", &dir, "unused for 40 days", NOW).unwrap();
    assert!(!dir.exists());
    let to = PathBuf::from(&m.to);
    assert!(to.join("SKILL.md").is_file() && to.join("references/a.md").is_file());
    assert_eq!(m.before_hash, m.after_hash, "a lossless move");
    assert_eq!((m.id, m.kind), (1, MutationKind::Archive));
    assert_eq!(a.archived().len(), 1);

    let r = a.restore("demo", NOW + 5).unwrap();
    assert_eq!((r.id, r.kind), (2, MutationKind::Restore));
    assert!(dir.join("references/a.md").is_file() && !to.exists());
    assert_eq!(r.after_hash, m.before_hash);
    assert!(a.archived().is_empty());
    assert_eq!(a.mutations().len(), 2, "the ledger only grows");
}

#[test]
fn restore_refuses_a_taken_place_a_changed_copy_and_an_unknown_name() {
    let root = tmp("refuse");
    let dir = skill_pkg(&root, "demo");
    let a = Archive::new(root.join(MUTATION_FILE), root.join(ARCHIVE_DIR));
    let m = a.archive("demo", &dir, "r", NOW).unwrap();
    assert!(a.restore("nope", NOW).is_err());

    skill_pkg(&root, "demo");
    assert!(a.restore("demo", NOW).unwrap_err().contains("exists now"));
    std::fs::remove_dir_all(&dir).unwrap();

    std::fs::write(PathBuf::from(&m.to).join("references/a.md"), "edited").unwrap();
    assert!(a
        .restore("demo", NOW)
        .unwrap_err()
        .contains("changed since"));
    assert!(!dir.exists(), "nothing moved on a refusal");
    assert_eq!(a.archived().len(), 1);
}

#[test]
fn archive_refuses_a_bad_name_a_missing_dir_and_a_symlink() {
    let root = tmp("badarch");
    let dir = skill_pkg(&root, "demo");
    let a = Archive::new(root.join(MUTATION_FILE), root.join(ARCHIVE_DIR));
    assert!(a.archive("../x", &dir, "r", NOW).is_err());
    assert!(a.archive("demo", &root.join("none"), "r", NOW).is_err());
    #[cfg(unix)]
    {
        let link = root.join("link");
        std::os::unix::fs::symlink(&dir, &link).unwrap();
        assert!(a.archive("demo", &link, "r", NOW).is_err());
        assert!(dir.exists(), "the target is untouched");
    }
    assert!(a.mutations().is_empty());
}

#[test]
fn the_curator_state_round_trips_and_fails_loud_when_malformed() {
    let dir = tmp("state");
    let path = dir.join(STATE_FILE);
    assert_eq!(CuratorState::load(&path).unwrap(), CuratorState::default());
    let mut s = CuratorState {
        paused: true,
        ..CuratorState::default()
    };
    s.pinned.insert("a".into());
    s.locked.insert("b".into());
    s.adopted.insert("c".into());
    s.save(&path).unwrap();
    assert_eq!(CuratorState::load(&path).unwrap(), s);
    std::fs::write(&path, "{").unwrap();
    assert!(CuratorState::load(&path).is_err());
    std::fs::write(&path, "{}").unwrap();
    assert_eq!(CuratorState::load(&path).unwrap(), CuratorState::default());
}

#[test]
fn a_ledger_that_cannot_be_written_undoes_the_move() {
    let root = tmp("noledger");
    let dir = skill_pkg(&root, "demo");
    // The ledger path is a directory: every append fails.
    let ledger = root.join("ledger-is-a-dir");
    std::fs::create_dir_all(&ledger).unwrap();
    let a = Archive::new(&ledger, root.join(ARCHIVE_DIR));
    let err = a.archive("demo", &dir, "r", NOW).unwrap_err();
    assert!(err.contains("the move was undone"), "{err}");
    assert!(dir.join("SKILL.md").is_file() && dir.join("references/a.md").is_file());
    assert!(a.archived().is_empty());
    // Nothing is left behind in the archive either.
    let left = std::fs::read_dir(root.join(ARCHIVE_DIR))
        .map(|d| d.count())
        .unwrap_or(0);
    assert_eq!(left, 0);
}

#[cfg(unix)]
#[test]
fn a_restore_whose_ledger_write_fails_is_undone_too() {
    use std::os::unix::fs::PermissionsExt;
    let root = tmp("norestoreledger");
    let dir = skill_pkg(&root, "demo");
    let ledger = root.join(MUTATION_FILE);
    let a = Archive::new(&ledger, root.join(ARCHIVE_DIR));
    let m = a.archive("demo", &dir, "r", NOW).unwrap();
    // Reads still work, appends fail.
    std::fs::set_permissions(&ledger, std::fs::Permissions::from_mode(0o444)).unwrap();
    if std::fs::OpenOptions::new()
        .append(true)
        .open(&ledger)
        .is_ok()
    {
        return; // running as a user the mode does not bind (root)
    }
    let err = a.restore("demo", NOW + 1).unwrap_err();
    assert!(err.contains("the move was undone"), "{err}");
    assert!(
        PathBuf::from(&m.to).join("references/a.md").is_file(),
        "back in the archive"
    );
    assert!(!dir.exists(), "not half restored");
    assert_eq!(
        a.archived().len(),
        1,
        "the ledger still says archived, and it is"
    );
}
