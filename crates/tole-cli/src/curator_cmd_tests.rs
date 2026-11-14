use super::*;
use tole_core::secrets::SecretSet;
use tole_core::skill_curator::{UsageKind, UsageRecord, MUTATION_FILE};
use tole_core::skill_draft::{gate, Dedup, HostContext, Promotion, Proposal, Trigger};

const DAY: u64 = 86_400;
const NOW: u64 = 200 * DAY;

struct Rig {
    root: PathBuf,
    drafts: DraftStore,
    usage: UsageLedger,
    archive: Archive,
    state: PathBuf,
    lock: PathBuf,
    n: u64,
}

impl Rig {
    fn new(tag: &str) -> Self {
        let root =
            std::env::temp_dir().join(format!("tole-curator-cli-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        Self {
            drafts: DraftStore::new(root.join("drafts")),
            usage: UsageLedger::new(root.join("usage.jsonl")),
            archive: Archive::new(root.join(MUTATION_FILE), root.join("archive")),
            state: root.join("state.json"),
            lock: root.join("curator.lock"),
            root,
            n: 0,
        }
    }

    fn skill_dir(&self, name: &str) -> PathBuf {
        self.root.join("skills").join(name)
    }

    fn write_skill(&self, name: &str) {
        let d = self.skill_dir(name);
        std::fs::create_dir_all(d.join("references")).unwrap();
        std::fs::write(
            d.join("SKILL.md"),
            format!("---\nname: {name}\ndescription: d\n---\nbody\n"),
        )
        .unwrap();
        std::fs::write(d.join("references/a.md"), "ref").unwrap();
    }

    /// An agent-grown skill: a promoted draft, `age` days old, last loaded
    /// `used` days ago (`None` = never).
    fn grown(&mut self, name: &str, age: u64, used: Option<u64>) {
        self.write_skill(name);
        self.n += 1;
        let md = format!("---\nname: {name}\ndescription: d\n---\nbody\n");
        let ctx = HostContext {
            session: Some("s".into()),
            model: None,
            taint: Vec::new(),
            dedup: Dedup::from_nearest(None),
            now: 1000 + self.n,
        };
        let p = Proposal {
            name: name.into(),
            skill_md: md,
            trigger: Trigger::DeadEnd,
            sources: Vec::new(),
        };
        let (d, md) = gate(&p, &ctx, &SecretSet::default()).unwrap();
        self.drafts.create(&d, &md).unwrap();
        self.drafts
            .promote(
                &d.id,
                Promotion {
                    package_hash: "sha256:x".into(),
                    dir: self.skill_dir(name).display().to_string(),
                    at: NOW - age * DAY,
                },
            )
            .unwrap();
        if let Some(days) = used {
            self.load(name, days);
        }
    }

    fn load(&self, name: &str, days_ago: u64) {
        self.usage
            .append(&UsageRecord {
                t: NOW - days_ago * DAY,
                skill: Some(name.into()),
                kind: UsageKind::Loaded,
            })
            .unwrap();
    }

    fn env(&self) -> Env<'_> {
        Env {
            drafts: &self.drafts,
            state_path: &self.state,
            usage: &self.usage,
            archive: &self.archive,
            lock_path: &self.lock,
            cron_skills: BTreeSet::new(),
            ws: Some(&self.root),
            thresholds: Thresholds::default(),
            enabled: true,
            now: NOW,
        }
    }
}

fn names(v: &[Mutation]) -> Vec<&str> {
    v.iter().map(|m| m.skill.as_str()).collect()
}

#[test]
fn a_sweep_archives_only_what_is_unused_past_the_threshold() {
    let mut r = Rig::new("sweep");
    r.grown("old", 90, Some(40));
    r.grown("recent", 90, Some(3));
    r.grown("fresh", 5, None);
    r.grown("stale", 90, Some(20));
    let rep = sweep(&r.env(), false).unwrap();
    assert_eq!(names(&rep.archived), ["old"]);
    assert!(!r.skill_dir("old").exists());
    for kept in ["recent", "fresh", "stale"] {
        assert!(r.skill_dir(kept).join("SKILL.md").exists(), "{kept}");
    }
    let text = render_report(&rep, false);
    assert!(
        text.contains("archived old (entry 1, restore with `tole curator restore old`)"),
        "{text}"
    );
    assert!(text.contains("stale  stale  unused for 20 days"), "{text}");
    // The package moved whole, references included.
    let to = PathBuf::from(&rep.archived[0].to);
    assert!(to.join("references/a.md").is_file());
    // Restoring puts it back exactly.
    r.archive.restore("old", NOW).unwrap();
    assert!(r.skill_dir("old").join("references/a.md").is_file());
}

#[test]
fn a_dry_run_and_a_paused_curator_change_nothing() {
    let mut r = Rig::new("dry");
    r.grown("old", 90, Some(40));
    let rep = sweep(&r.env(), true).unwrap();
    assert_eq!(rep.would_archive, ["old"]);
    assert!(rep.archived.is_empty() && r.skill_dir("old").exists());
    assert!(render_report(&rep, true).contains("would archive: old"));

    CuratorState {
        paused: true,
        ..CuratorState::default()
    }
    .save(&r.state)
    .unwrap();
    let rep = sweep(&r.env(), false).unwrap();
    assert!(rep.held.as_deref().unwrap().contains("paused"));
    assert_eq!(rep.would_archive, ["old"]);
    assert!(r.skill_dir("old").exists() && r.archive.mutations().is_empty());
}

#[test]
fn pinned_cron_named_and_unmanaged_skills_are_left_alone() {
    let mut r = Rig::new("protect");
    r.grown("pinned", 90, Some(60));
    r.grown("cron", 90, Some(60));
    r.grown("open", 90, Some(60));
    r.write_skill("hand"); // hand-written: not a promoted draft
    let mut st = CuratorState::default();
    st.pinned.insert("pinned".into());
    st.save(&r.state).unwrap();
    let mut env = r.env();
    env.cron_skills.insert("cron".into());
    let rep = sweep(&env, false).unwrap();
    assert_eq!(names(&rep.archived), ["open"]);
    for kept in ["pinned", "cron", "hand"] {
        assert!(r.skill_dir(kept).exists(), "{kept}");
    }

    // Adopting the hand-written skill puts it under curation.
    let msg = edit_state(&r.state, "hand", |s| &mut s.adopted, true, "adopted").unwrap();
    assert_eq!(msg, "hand is adopted");
    let rep = sweep(&r.env(), false).unwrap();
    assert!(
        rep.assessments.iter().any(|a| a.name == "hand"),
        "{:?}",
        rep.assessments
    );
}

#[test]
fn only_one_sweep_runs_at_a_time_and_a_stale_lock_is_taken_over() {
    let mut r = Rig::new("lock");
    r.grown("old", 90, Some(40));
    std::fs::write(&r.lock, "").unwrap();
    assert!(sweep(&r.env(), false)
        .unwrap_err()
        .contains("another curator sweep"));
    assert!(r.skill_dir("old").exists());

    let old = std::time::SystemTime::UNIX_EPOCH
        + std::time::Duration::from_secs(NOW - 2 * LOCK_STALE_SECS);
    std::fs::OpenOptions::new()
        .write(true)
        .open(&r.lock)
        .unwrap()
        .set_modified(old)
        .unwrap();
    // `now` is synthetic; the lock's mtime is compared against it.
    let rep = sweep(&r.env(), false).unwrap();
    assert_eq!(names(&rep.archived), ["old"]);
    assert!(!r.lock.exists(), "the lock is released");
}

#[test]
fn an_archive_failure_is_reported_not_hidden() {
    let mut r = Rig::new("fail");
    r.grown("old", 90, Some(40));
    r.grown("ok", 90, Some(40));
    // `old` becomes a symlink: the archive refuses it.
    #[cfg(unix)]
    {
        let real = r.skill_dir("old");
        let moved = r.root.join("moved");
        std::fs::rename(&real, &moved).unwrap();
        std::os::unix::fs::symlink(&moved, &real).unwrap();
        let rep = sweep(&r.env(), false).unwrap();
        assert_eq!(names(&rep.archived), ["ok"]);
        assert_eq!(rep.errors.len(), 1, "{:?}", rep.errors);
        assert!(render_report(&rep, false).contains("error: old"));
    }
}

#[test]
fn status_summarises_the_library_and_the_state() {
    let mut r = Rig::new("status");
    r.grown("a", 90, Some(1));
    r.grown("b", 90, Some(20));
    r.grown("c", 90, Some(50));
    r.grown("p", 90, Some(50));
    let mut st = CuratorState::default();
    st.pinned.insert("p".into());
    st.save(&r.state).unwrap();
    sweep(&r.env(), false).unwrap();
    let text = render_status(&r.env()).unwrap();
    assert!(text.contains("curator: on"), "{text}");
    assert!(
        text.contains(
            "managed: 3 (1 active, 1 stale, 0 due for archive; 1 pinned or cron-named left alone)"
        ),
        "{text}"
    );
    assert!(
        text.contains("pinned: p") && text.contains("archived: c (entry 1"),
        "{text}"
    );
    assert!(
        text.contains("stale after 14 days, archive after 30 days"),
        "{text}"
    );
}

#[test]
fn state_edits_are_idempotent_and_refuse_a_bad_name() {
    let r = Rig::new("state");
    let pin = |add| edit_state(&r.state, "demo", |s| &mut s.pinned, add, "pinned");
    assert_eq!(pin(true).unwrap(), "demo is pinned");
    assert_eq!(pin(true).unwrap(), "demo is already pinned");
    assert_eq!(pin(false).unwrap(), "demo is no longer pinned");
    assert_eq!(pin(false).unwrap(), "demo is already not pinned");
    assert!(edit_state(&r.state, "../x", |s| &mut s.locked, true, "locked").is_err());
    assert!(CuratorState::load(&r.state).unwrap().pinned.is_empty());
}

#[test]
fn invalid_thresholds_stop_the_sweep() {
    let mut r = Rig::new("thr");
    r.grown("old", 90, Some(40));
    let mut env = r.env();
    env.thresholds = Thresholds {
        stale_days: 30,
        archive_days: 14,
        grace_days: 0,
    };
    assert!(sweep(&env, false).is_err());
    assert!(r.skill_dir("old").exists());
}

#[test]
fn a_skill_doing_worse_than_skill_less_runs_is_proposed_never_archived() {
    let mut r = Rig::new("under");
    r.grown("weak", 90, Some(1));
    let rec = |skill: Option<&str>, kind| UsageRecord {
        t: NOW - DAY,
        skill: skill.map(String::from),
        kind,
    };
    for _ in 0..8 {
        r.usage.append(&rec(None, UsageKind::Success)).unwrap();
    }
    for _ in 0..2 {
        r.usage.append(&rec(None, UsageKind::Failure)).unwrap();
    }
    for _ in 0..2 {
        r.usage
            .append(&rec(Some("weak"), UsageKind::Success))
            .unwrap();
    }
    for _ in 0..8 {
        r.usage
            .append(&rec(Some("weak"), UsageKind::Failure))
            .unwrap();
    }
    let rep = sweep(&r.env(), false).unwrap();
    assert!(rep.archived.is_empty() && r.skill_dir("weak").exists());
    let text = render_report(&rep, false);
    assert!(
        text.contains(
            "proposal: review weak: it succeeds 20% of 10 runs, runs without a skill 80%"
        ),
        "{text}"
    );
}

#[test]
fn a_switched_off_curator_changes_nothing_and_says_why() {
    let mut r = Rig::new("off");
    r.grown("old", 90, Some(40));
    let mut env = r.env();
    env.enabled = false;
    let rep = sweep(&env, false).unwrap();
    assert!(
        rep.held.as_deref().unwrap().contains("switched off"),
        "{:?}",
        rep.held
    );
    assert_eq!(rep.would_archive, ["old"]);
    assert!(r.skill_dir("old").exists() && r.archive.mutations().is_empty());
    assert!(render_status(&env)
        .unwrap()
        .contains("curator: off (curator.enabled = false"));
    // A dry run still reports.
    assert_eq!(sweep(&env, true).unwrap().would_archive, ["old"]);
}

#[cfg(feature = "shell-tools")]
mod scheduling {
    use super::*;
    use crate::curator_cmd::schedule;

    fn dirs(tag: &str) -> (PathBuf, PathBuf) {
        let root =
            std::env::temp_dir().join(format!("tole-curator-sched-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let (cron, work) = (root.join("cron"), root.join("work"));
        std::fs::create_dir_all(&cron).unwrap();
        std::fs::create_dir_all(&work).unwrap();
        (cron, work)
    }

    #[test]
    fn the_weekly_job_is_a_zero_token_script_saved_once_and_removable() {
        let (cron, work) = dirs("once");
        assert!(schedule::line(&cron).contains("not scheduled"));
        let msg = schedule::install(&cron, None, &work, 1_700_000_000).unwrap();
        assert!(
            msg.contains("0 4 * * 1") && msg.contains("tole serve --cron"),
            "{msg}"
        );
        let job = schedule::find(&cron).unwrap();
        assert_eq!(
            (job.id.as_str(), job.mode.as_str(), job.state),
            ("curator-weekly", "script", "active")
        );
        assert!(schedule::line(&cron).contains("0 4 * * 1"));
        // argv, no shell, no path: the scheduler refuses a path-qualified program.
        let l = crate::cron::store::open_read(&cron).unwrap();
        let live = crate::cron::resolve(l.state(), "curator-weekly").unwrap();
        let script = crate::cron::field(&live, "script").unwrap().to_string();
        assert_eq!(script, "tole curator run");
        let argv = tole_core::run_command::RunCommandTool::split_argv(&script).unwrap();
        assert_eq!(argv, ["tole", "curator", "run"]);
        // Saved once.
        let again = schedule::install(&cron, None, &work, 1_700_000_001).unwrap_err();
        assert!(again.to_string().contains("already exists"), "{again}");
        // Removed, then it can be saved again with another schedule.
        assert_eq!(
            schedule::remove(&cron, 1_700_000_002).unwrap(),
            "removed curator-weekly"
        );
        assert!(schedule::find(&cron).is_none());
        assert!(schedule::remove(&cron, 1_700_000_003).is_err());
        schedule::install(&cron, Some("30 3 * * 0"), &work, 1_700_000_004).unwrap();
        assert!(schedule::line(&cron).contains("30 3 * * 0"));
    }

    #[test]
    fn a_bad_schedule_or_a_relative_workdir_is_refused() {
        let (cron, work) = dirs("bad");
        assert!(schedule::install(&cron, Some("not a cron"), &work, 1).is_err());
        assert!(schedule::install(&cron, None, Path::new("relative"), 1).is_err());
        assert!(schedule::find(&cron).is_none());
    }
}
