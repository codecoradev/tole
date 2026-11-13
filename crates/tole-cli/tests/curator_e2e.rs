//! #593 end-to-end with the REAL binary: `tole curator` reports, pins,
//! sweeps, restores and pauses over a hermetic `CODECORA_HOME`.

use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use tole_core::secrets::SecretSet;
use tole_core::skill_curator::{UsageKind, UsageLedger, UsageRecord};
use tole_core::skill_draft::{gate, Dedup, DraftStore, HostContext, Promotion, Proposal, Trigger};

static COUNTER: AtomicU64 = AtomicU64::new(0);
const DAY: u64 = 86_400;

fn tmpdir() -> PathBuf {
    let n = COUNTER.fetch_add(1, Ordering::SeqCst);
    let d = std::env::temp_dir().join(format!("tole-curator-e2e-{}-{n}", std::process::id()));
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

fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

/// An agent-grown skill (a promoted draft) last loaded `used_days` ago.
fn grown(home: &Path, name: &str, used_days: u64) {
    let dir = home.join("tole").join("skills").join(name);
    std::fs::create_dir_all(dir.join("references")).unwrap();
    let md = format!("---\nname: {name}\ndescription: d\n---\nbody\n");
    std::fs::write(dir.join("SKILL.md"), &md).unwrap();
    std::fs::write(dir.join("references/a.md"), "ref").unwrap();
    let store = DraftStore::new(home.join("tole").join("skill-drafts"));
    let ctx = HostContext {
        session: Some("s".into()),
        model: None,
        taint: Vec::new(),
        dedup: Dedup::from_nearest(None),
        now: now() - 100 * DAY + COUNTER.fetch_add(1, Ordering::SeqCst),
    };
    let p = Proposal {
        name: name.into(),
        skill_md: md,
        trigger: Trigger::DeadEnd,
        sources: Vec::new(),
    };
    let (d, md) = gate(&p, &ctx, &SecretSet::default()).unwrap();
    store.create(&d, &md).unwrap();
    store
        .promote(
            &d.id,
            Promotion {
                package_hash: "sha256:x".into(),
                dir: dir.display().to_string(),
                at: now() - 100 * DAY,
            },
        )
        .unwrap();
    UsageLedger::new(home.join("tole").join("skill-usage.jsonl"))
        .append(&UsageRecord {
            t: now() - used_days * DAY,
            skill: Some(name.into()),
            kind: UsageKind::Loaded,
        })
        .unwrap();
}

fn skill_dir(home: &Path, name: &str) -> PathBuf {
    home.join("tole").join("skills").join(name)
}

#[test]
fn an_empty_library_reports_nothing_to_manage() {
    let home = tmpdir();
    let o = tole(&home, &["curator", "status"]);
    assert!(o.status.success(), "{}", out(&o));
    assert!(
        out(&o).contains("curator: on") && out(&o).contains("managed: 0"),
        "{}",
        out(&o)
    );
    let o = tole(&home, &["curator", "run"]);
    assert!(
        o.status.success() && out(&o).contains("no managed skills"),
        "{}",
        out(&o)
    );
}

#[test]
fn a_sweep_archives_the_idle_skill_and_restore_brings_it_back() {
    let home = tmpdir();
    grown(&home, "idle", 45);
    grown(&home, "busy", 2);

    let o = tole(&home, &["curator", "run", "--dry-run"]);
    assert!(o.status.success(), "{}", out(&o));
    assert!(out(&o).contains("would archive: idle"), "{}", out(&o));
    assert!(skill_dir(&home, "idle").exists(), "a dry run moves nothing");

    let o = tole(&home, &["curator", "run"]);
    assert!(o.status.success(), "{}", out(&o));
    assert!(out(&o).contains("archived idle (entry 1"), "{}", out(&o));
    assert!(!skill_dir(&home, "idle").exists() && skill_dir(&home, "busy").exists());
    let o = tole(&home, &["curator", "status"]);
    assert!(out(&o).contains("archived: idle (entry 1"), "{}", out(&o));

    let o = tole(&home, &["curator", "restore", "idle"]);
    assert!(o.status.success(), "{}", out(&o));
    assert!(skill_dir(&home, "idle").join("references/a.md").is_file());
    let o = tole(&home, &["curator", "restore", "idle"]);
    assert!(
        !o.status.success() && out(&o).contains("no archived skill"),
        "{}",
        out(&o)
    );
}

#[test]
fn pin_and_pause_hold_a_sweep_back() {
    let home = tmpdir();
    grown(&home, "idle", 45);

    let o = tole(&home, &["curator", "pin", "idle"]);
    assert!(
        o.status.success() && out(&o).contains("idle is pinned"),
        "{}",
        out(&o)
    );
    let o = tole(&home, &["curator", "run"]);
    assert!(
        o.status.success() && !out(&o).contains("archived idle"),
        "{}",
        out(&o)
    );
    assert!(skill_dir(&home, "idle").exists(), "pinned stays");

    let o = tole(&home, &["curator", "unpin", "idle"]);
    assert!(out(&o).contains("no longer pinned"), "{}", out(&o));
    let o = tole(&home, &["curator", "pause"]);
    assert!(out(&o).contains("paused"), "{}", out(&o));
    let o = tole(&home, &["curator", "run"]);
    assert!(out(&o).contains("the curator is paused"), "{}", out(&o));
    assert!(skill_dir(&home, "idle").exists(), "paused changes nothing");
    let o = tole(&home, &["curator", "status"]);
    assert!(out(&o).contains("curator: paused"), "{}", out(&o));

    tole(&home, &["curator", "resume"]);
    let o = tole(&home, &["curator", "run"]);
    assert!(out(&o).contains("archived idle"), "{}", out(&o));
}

#[test]
fn a_locked_skill_and_a_bad_name_are_handled() {
    let home = tmpdir();
    let o = tole(&home, &["curator", "lock", "demo"]);
    assert!(
        o.status.success() && out(&o).contains("demo is locked"),
        "{}",
        out(&o)
    );
    let o = tole(&home, &["curator", "pin", "../x"]);
    assert!(
        !o.status.success() && out(&o).contains("invalid skill name"),
        "{}",
        out(&o)
    );
    let o = tole(&home, &["curator", "status"]);
    assert!(out(&o).contains("locked: demo"), "{}", out(&o));
}
