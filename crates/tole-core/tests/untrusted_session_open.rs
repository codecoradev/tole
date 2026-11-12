//! A session file in a directory another process can write (a cron job's
//! workdir, #581) is opened on one handle that never follows a symlink,
//! never blocks on a FIFO, must be a regular file and, through
//! `open_bounded`, is size-capped. The race these tests replay: a caller
//! validates the path, the file is swapped, then the caller opens it.

#![cfg(unix)]

use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::Duration;

use serde_json::json;
use tole_core::delegation_report::read_plain_file;
use tole_core::entry::{EntryType, NewEntry};
use tole_core::storage::{Commit, JsonlStorage, Storage};

fn tmpdir(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("tole-untrusted-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn message(text: &str) -> Commit {
    Commit::new().entry(NewEntry::root(
        EntryType::new(EntryType::MESSAGE),
        json!({"role": "user", "text": text}),
    ))
}

/// A valid one-message session `s1` in `dir`; returns its path.
fn session(dir: &Path) -> PathBuf {
    let mut s = JsonlStorage::create(dir, "s1", None).unwrap();
    s.commit(message("hello")).unwrap();
    dir.join("s1.jsonl")
}

/// Runs `f` on a thread; panics if it has not returned within 5 s.
fn without_blocking<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> T {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(f());
    });
    rx.recv_timeout(Duration::from_secs(5))
        .expect("the open must not block")
}

#[test]
fn a_session_swapped_for_a_symlink_after_the_check_is_refused_at_the_open() {
    for bounded in [false, true] {
        let dir = tmpdir(&format!("symlink-{bounded}"));
        let path = session(&dir);
        // The check passes on the real file ...
        assert!(read_plain_file(&path, 1 << 20).unwrap().is_some());
        // ... then the file is swapped for a symlink to one outside.
        let outside = dir.join("outside.jsonl");
        std::fs::rename(&path, &outside).unwrap();
        std::os::unix::fs::symlink(&outside, &path).unwrap();
        let before = std::fs::read(&outside).unwrap();
        let opened = if bounded {
            JsonlStorage::open_bounded(&path, 1 << 20)
        } else {
            JsonlStorage::open(&path)
        };
        if let Ok(mut s) = opened {
            let _ = s.commit(message("through the link"));
            panic!("bounded={bounded}: the open followed the symlink");
        }
        assert_eq!(std::fs::read(&outside).unwrap(), before);
    }
}

/// A writable open of a FIFO does not wait on Linux, but a read-only one
/// does without `O_NONBLOCK`, and reading a FIFO the reader also holds open
/// for writing waits for data: the regular-file check refuses it first.
#[test]
fn a_session_swapped_for_a_fifo_is_refused_without_blocking() {
    for mode in ["open", "bounded", "read_only"] {
        let dir = tmpdir(&format!("fifo-{mode}"));
        let path = session(&dir);
        std::fs::remove_file(&path).unwrap();
        let made = std::process::Command::new("mkfifo")
            .arg(&path)
            .status()
            .unwrap();
        assert!(made.success());
        let r = without_blocking(move || {
            let opened = match mode {
                "open" => JsonlStorage::open(&path),
                "bounded" => JsonlStorage::open_bounded(&path, 1 << 20),
                _ => JsonlStorage::open_read_only(&path),
            };
            opened.err().map(|e| e.to_string())
        });
        assert!(r.is_some(), "{mode}: a FIFO must not open");
    }
}

#[test]
fn a_symlinked_lock_sidecar_is_refused_and_its_target_left_alone() {
    let dir = tmpdir("lock");
    let path = session(&dir);
    let lock = dir.join("s1.jsonl.lock");
    let _ = std::fs::remove_file(&lock);
    let victim = dir.join("victim.txt");
    std::fs::write(&victim, "keep me").unwrap();
    std::os::unix::fs::symlink(&victim, &lock).unwrap();
    assert!(JsonlStorage::open(&path).is_err());
    assert_eq!(std::fs::read_to_string(&victim).unwrap(), "keep me");
}

#[test]
fn compaction_never_writes_through_a_planted_temp_symlink() {
    let dir = tmpdir("compact");
    let path = session(&dir);
    let mut s = JsonlStorage::open(&path).unwrap();
    let victim = dir.join("victim.txt");
    std::fs::write(&victim, "keep me").unwrap();
    std::os::unix::fs::symlink(&victim, dir.join("s1.jsonl.tmp")).unwrap();
    let _ = s.compact();
    assert_eq!(std::fs::read_to_string(&victim).unwrap(), "keep me");
}

#[test]
fn open_bounded_refuses_a_session_over_the_cap() {
    let dir = tmpdir("cap");
    let path = session(&dir);
    let len = std::fs::metadata(&path).unwrap().len();
    assert!(JsonlStorage::open_bounded(&path, len - 1).is_err());
    let mut s = JsonlStorage::open_bounded(&path, len).unwrap();
    assert_eq!(s.entries().len(), 1);
    s.commit(message("still writable")).unwrap();
    drop(s);
    assert_eq!(JsonlStorage::open(&path).unwrap().entries().len(), 2);
}
