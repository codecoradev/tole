//! The read-only size report of a sessions directory (#1020, step 2 of
//! `docs/session-retention-design.md`): `tole_cli::session::size` over a real
//! directory, and the lines the real binary prints for `tole sessions` and
//! `tole doctor`. The report must never change the directory.

mod common;
use common::TestDir;
use std::path::Path;
use std::process::{Output, Stdio};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tole_cli::session::size::{human, scan};

/// Write `<name>` with `len` bytes and set its mtime to `age_days` ago.
fn file(dir: &Path, name: &str, len: usize, age_days: u64) {
    let p = dir.join(name);
    std::fs::write(&p, vec![b'x'; len]).unwrap();
    let at = SystemTime::now() - Duration::from_secs(age_days * 86400);
    std::fs::File::options()
        .write(true)
        .open(&p)
        .unwrap()
        .set_modified(at)
        .unwrap();
}

/// Every name in `dir` with its length and mtime, to prove nothing changed.
fn snapshot(dir: &Path) -> Vec<(String, u64, SystemTime)> {
    let mut v: Vec<_> = std::fs::read_dir(dir)
        .unwrap()
        .flatten()
        .map(|e| {
            let m = e.metadata().unwrap();
            (
                e.file_name().to_string_lossy().into_owned(),
                m.len(),
                m.modified().unwrap(),
            )
        })
        .collect();
    v.sort();
    v
}

#[test]
fn it_counts_total_largest_and_oldest_by_last_write_and_changes_nothing() {
    let d = TestDir::new("tole-size-report");
    file(&d, "s-new.jsonl", 100, 1);
    file(&d, "s-old.jsonl", 300, 90);
    file(&d, "s-big.jsonl", 5000, 10);
    // Not session files: a lock sidecar, a temp file, another extension, an
    // invalid id, a directory and a symlink named like a session.
    file(&d, "s-new.jsonl.lock", 1, 400);
    file(&d, "s-new.jsonl.tmp", 9999, 400);
    file(&d, "notes.txt", 9999, 400);
    file(&d, "bad id.jsonl", 9999, 400);
    std::fs::create_dir(d.join("sub.jsonl")).unwrap();
    #[cfg(unix)]
    std::os::unix::fs::symlink("/etc/hostname", d.join("link.jsonl")).unwrap();
    let before = snapshot(&d);

    let r = scan(&d).unwrap();
    assert_eq!((r.files, r.bytes), (3, 5400));
    assert_eq!(r.largest, Some(("s-big".into(), 5000)));
    let (id, at) = r.oldest.clone().unwrap();
    assert_eq!(id, "s-old");
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs();
    assert!(
        (now - at).abs_diff(90 * 86400) < 120,
        "mtime of the oldest: {at}"
    );
    let line = r.line().unwrap();
    assert!(
        line.starts_with("store: 3 session file(s), 5.3 KiB total; largest s-big (4.9 KiB); oldest s-old (last written 20"),
        "{line}"
    );
    assert_eq!(snapshot(&d), before, "the scan is read-only");
}

#[test]
fn an_empty_or_missing_directory_has_no_line_and_ties_pick_the_smaller_id() {
    let d = TestDir::new("tole-size-report-empty");
    assert_eq!(scan(&d).unwrap().line(), None);
    assert_eq!(scan(&d.join("missing")), None);
    file(&d, "b.jsonl", 10, 5);
    file(&d, "a.jsonl", 10, 5);
    let r = scan(&d).unwrap();
    assert_eq!(r.largest.as_ref().map(|l| l.0.as_str()), Some("a"));
    // mtimes were set from two `now()` calls a moment apart: only the size tie is exact.
    assert_eq!(r.files, 2);
}

#[test]
fn bytes_are_shown_in_binary_units() {
    assert_eq!(human(0), "0 B");
    assert_eq!(human(1023), "1023 B");
    assert_eq!(human(1024), "1.0 KiB");
    assert_eq!(human(1536 * 1024), "1.5 MiB");
    assert_eq!(human(3 * 1024 * 1024 * 1024), "3.0 GiB");
    assert_eq!(human(5000 * 1024 * 1024 * 1024), "5000.0 GiB");
}

fn tole(cwd: &Path, home: &Path, args: &[&str]) -> Output {
    let mut c = common::hermetic_command(env!("CARGO_BIN_EXE_tole"), home, &[]);
    c.args(args)
        .current_dir(cwd)
        .env("TOLE_NO_UPDATE_CHECK", "1")
        .stdin(Stdio::null());
    for k in [
        "TOLE_BASE_URL",
        "TOLE_API_KEY",
        "TOLE_MODEL",
        "OPENAI_BASE_URL",
        "OPENAI_API_KEY",
        "OPENAI_MODEL",
    ] {
        c.env_remove(k);
    }
    c.output().unwrap()
}

#[test]
fn sessions_and_doctor_print_the_report_and_leave_the_directory_alone() {
    let d = TestDir::new("tole-size-report-cli");
    let home = TestDir::new("tole-size-report-home");
    let sessions = d.join("s");
    std::fs::create_dir(&sessions).unwrap();
    // `sessions` skips a file that does not replay; the report counts its bytes.
    file(&sessions, "s-a.jsonl", 2048, 3);
    file(&sessions, "s-b.jsonl", 1024, 30);
    // One real session, so `sessions` has a row to list.
    drop(tole_core::storage::JsonlStorage::create(&sessions, "s-real", None).unwrap());
    let before = snapshot(&sessions);
    let dir = sessions.to_string_lossy().into_owned();
    let stdout = |o: &Output| String::from_utf8_lossy(&o.stdout).into_owned();

    // `tole sessions`: the table stays on stdout as it was; the report is one
    // line on stderr (a script that parses the table never sees it).
    let o = tole(&d, &home, &["--sessions-dir", &dir, "sessions"]);
    let (out, err) = (stdout(&o), String::from_utf8_lossy(&o.stderr).into_owned());
    assert!(
        out.contains("s-real") && !out.contains("s-a") && !out.contains("store:"),
        "{out}"
    );
    let line = err.lines().find(|l| l.starts_with("store: ")).expect(&err);
    assert!(
        line.starts_with("store: 3 session file(s), ")
            && line.contains("largest s-a (2.0 KiB); oldest s-b (last written 20"),
        "{err}"
    );

    let out = stdout(&tole(
        &d,
        &home,
        &[
            "--sessions-dir",
            &dir,
            "doctor",
            "--tier",
            "offline",
            "--no-ledger",
        ],
    ));
    assert!(out.contains("\nstore: 3 session file(s), "), "{out}");
    assert_eq!(
        snapshot(&sessions),
        before,
        "doctor and sessions are read-only"
    );
}
