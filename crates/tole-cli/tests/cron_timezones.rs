//! Per-job time zones on the CLI (#580 part 2): `--timezone` on create and
//! edit, the zone source (`$TZDIR` here), `list` in local time and UTC, and
//! jobs without a zone exactly as before.

#![cfg(feature = "shell-tools")]

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use tole_cli::cron::{self, store, CreateSpec, EditSpec, TickOptions};

/// 2026-01-01T00:00:00Z.
const T: i64 = 1_767_225_600;

/// A TZif v2 file with no transitions: one offset and a POSIX footer.
fn tzif(offset: i32, footer: &str) -> Vec<u8> {
    let header = || {
        let mut h = b"TZif2".to_vec();
        h.extend([0u8; 15]);
        for n in [0u32, 0, 0, 0, 1, 4] {
            h.extend(n.to_be_bytes());
        }
        h
    };
    let body = |out: &mut Vec<u8>| {
        out.extend(offset.to_be_bytes());
        out.extend([0, 0]);
        out.extend(b"ZZZ\0");
    };
    let mut out = header();
    body(&mut out);
    out.extend(header());
    body(&mut out);
    out.extend(format!("\n{footer}\n").bytes());
    out
}

/// A private zoneinfo (`$TZDIR`) with `Asia/Jakarta`, and no user config.
fn env() {
    static ONCE: OnceLock<()> = OnceLock::new();
    ONCE.get_or_init(|| {
        let base = std::env::temp_dir().join(format!("tole-cron-tz-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(base.join("zoneinfo/Asia")).unwrap();
        std::fs::create_dir_all(base.join("home")).unwrap();
        std::fs::write(
            base.join("zoneinfo/Asia/Jakarta"),
            tzif(7 * 3_600, "<+07>-7"),
        )
        .unwrap();
        std::env::set_var("TZDIR", base.join("zoneinfo"));
        std::env::set_var("CODECORA_HOME", base.join("home"));
    });
}

fn tmp(tag: &str) -> (PathBuf, PathBuf) {
    let d = std::env::temp_dir().join(format!("tole-cron-tz-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(d.join("w")).unwrap();
    let d = d.canonicalize().unwrap();
    (d.join("cron"), d.join("w"))
}

fn script(work: &Path, id: &str, tz: Option<&str>) -> CreateSpec {
    CreateSpec {
        id: Some(id.into()),
        schedule: "0 9 * * *".into(),
        script: Some("echo hi".into()),
        workdir: work.to_path_buf(),
        timezone: tz.map(str::to_string),
        ..CreateSpec::default()
    }
}

#[test]
fn a_job_runs_on_its_zones_wall_clock_and_lists_both_times() {
    env();
    let (dir, work) = tmp("wall");
    for (id, tz) in [("off", "+07:00"), ("iana", "Asia/Jakarta")] {
        let job = cron::create(&dir, script(&work, id, Some(tz)), T).unwrap();
        assert_eq!(job.timezone.as_deref(), Some(tz));
        let l = store::open_read(&dir).unwrap();
        assert_eq!(l.state().next_run(id), Some(T + 2 * 3_600), "{id}");
    }
    let rows = cron::list(&dir).unwrap();
    let row = rows.iter().find(|r| r.id == "iana").unwrap();
    assert_eq!(row.timezone, "Asia/Jakarta");
    assert_eq!(
        row.next,
        "2026-01-01 09:00 Asia/Jakarta (2026-01-01 02:00Z)"
    );
    // The fire records the zone it was placed in.
    let r = cron::tick(&dir, T + 2 * 3_600, &TickOptions::standalone(2)).unwrap();
    assert_eq!(r.ran.len(), 2, "{r:?}");
    let l = store::open_read(&dir).unwrap();
    let fire = l.state().fire(&r.ran[0].0).unwrap();
    assert_eq!(fire.tz.as_ref().unwrap().offset, 7 * 3_600);
}

#[test]
fn a_job_without_a_zone_keeps_plain_utc_and_the_same_records() {
    env();
    let (dir, work) = tmp("plain");
    let job = cron::create(&dir, script(&work, "p", None), T).unwrap();
    assert!(job.timezone.is_none());
    let row = &cron::list(&dir).unwrap()[0];
    assert_eq!(
        (row.timezone.as_str(), row.next.as_str()),
        ("UTC", "2026-01-01 09:00Z")
    );
    let r = cron::tick(&dir, T + 9 * 3_600, &TickOptions::standalone(2)).unwrap();
    assert_eq!(r.ran.len(), 1);
    let log = std::fs::read_to_string(dir.join("jobs.jsonl")).unwrap();
    assert!(!log.contains("\"tz\""), "{log}");
    assert!(!log.contains("\"timezone\":\""), "{log}");
}

#[test]
fn an_unknown_or_invalid_zone_is_refused_on_save_with_a_remedy() {
    env();
    let (dir, work) = tmp("bad");
    for (tz, want) in [
        ("Mars/Olympus", "zoneinfo"),
        ("+25:00", "+07:00"),
        ("../etc/passwd", "not a valid zone name"),
        ("", "not a valid zone name"),
    ] {
        let e = cron::create(&dir, script(&work, "b", Some(tz)), T)
            .unwrap_err()
            .to_string();
        assert!(e.contains("--timezone") && e.contains(want), "{tz}: {e}");
    }
    assert!(cron::list(&dir).unwrap().is_empty(), "nothing was saved");
    // A hand-written line with a zone the source cannot resolve never runs
    // and never panics: the job is listed without a next run.
    let mut job = cron::create(&dir, script(&work, "h", None), T).unwrap();
    job.timezone = Some("Mars/Olympus".into());
    job.at = T + 1;
    store::with_lock(&dir, |l| l.put_job(job).map_err(anyhow::Error::from)).unwrap_err();
    let line = r#"{"v":1,"kind":"job","id":"h","schedule":"0 9 * * *","timezone":"Mars/Olympus","catch_up":"skip","grace_minutes":0,"paused":false,"removed":false,"at":1767225601,"mode":"script","script":"echo hi","created_by":"cli","workdir":"/"}"#;
    let mut log = std::fs::read_to_string(dir.join("jobs.jsonl")).unwrap();
    log.push_str(line);
    log.push('\n');
    std::fs::write(dir.join("jobs.jsonl"), log).unwrap();
    let rows = cron::list(&dir).unwrap();
    assert_eq!(rows.iter().find(|r| r.id == "h").unwrap().next, "-");
    let r = cron::tick(&dir, T + 9 * 3_600, &TickOptions::standalone(2)).unwrap();
    assert!(r.ran.is_empty(), "{r:?}");
}

#[test]
fn edit_keeps_or_changes_the_zone() {
    env();
    let (dir, work) = tmp("edit");
    cron::create(&dir, script(&work, "e", Some("+07:00")), T).unwrap();
    let renamed = EditSpec {
        name: Some("renamed".into()),
        ..EditSpec::default()
    };
    let j = cron::edit(&dir, "e", renamed, T + 10).unwrap();
    assert_eq!(j.timezone.as_deref(), Some("+07:00"));
    let utc = EditSpec {
        timezone: Some("UTC".into()),
        ..EditSpec::default()
    };
    let j = cron::edit(&dir, "e", utc, T + 20).unwrap();
    assert_eq!(j.timezone.as_deref(), Some("UTC"));
    let l = store::open_read(&dir).unwrap();
    assert_eq!(l.state().next_run("e"), Some(T + 9 * 3_600));
}
