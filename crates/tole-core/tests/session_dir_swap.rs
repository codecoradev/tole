//! #639: a directory on the session path that a job swaps for a symlink is
//! not followed. The storage holds its directory open without following
//! links, so the session file, its `.lock` sidecar and the compaction temp
//! file all stay where the session was created or opened.

#![cfg(unix)]

use serde_json::json;
use std::path::{Path, PathBuf};
use tole_core::entry::{EntryType, NewEntry};
use tole_core::storage::{Commit, JsonlStorage, Storage};

fn base(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("tole-dirswap-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d.canonicalize().unwrap()
}

fn msg(n: u64) -> NewEntry {
    NewEntry::root(EntryType::new(EntryType::MESSAGE), json!({ "n": n }))
}

fn names(dir: &Path) -> Vec<String> {
    let mut v: Vec<String> = std::fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    v.sort();
    v
}

/// `.tole/sessions` swapped for a link to a directory that also has a
/// `sessions` dir, the worst case for the old path-based code.
fn swap(b: &Path) -> PathBuf {
    let elsewhere = b.join("elsewhere");
    std::fs::create_dir_all(elsewhere.join("sessions")).unwrap();
    std::fs::rename(b.join(".tole"), b.join("moved")).unwrap();
    std::os::unix::fs::symlink(&elsewhere, b.join(".tole")).unwrap();
    elsewhere
}

#[test]
fn a_created_session_stays_put_when_its_directory_is_swapped_for_a_link() {
    let b = base("create");
    let sessions = b.join(".tole").join("sessions");
    std::fs::create_dir_all(&sessions).unwrap();
    let mut s = JsonlStorage::create(&sessions, "s", None).unwrap();
    s.commit(Commit::new().entry(msg(1))).unwrap();

    let elsewhere = swap(&b);
    s.commit(Commit::new().entry(msg(2))).unwrap();
    s.compact().unwrap();
    s.commit(Commit::new().entry(msg(3))).unwrap();

    assert!(
        names(&elsewhere.join("sessions")).is_empty(),
        "nothing created through the link: {:?}",
        names(&elsewhere.join("sessions"))
    );
    assert_eq!(names(&elsewhere), ["sessions"]);
    // Everything is where the session was created.
    assert_eq!(
        names(&b.join("moved/sessions")),
        ["s.jsonl", "s.jsonl.lock"]
    );
    let text = std::fs::read_to_string(b.join("moved/sessions/s.jsonl")).unwrap();
    for n in 1..=3 {
        assert!(
            text.contains(&format!("\"n\":{n}")),
            "entry {n} is in the file"
        );
    }
    drop(s);
    let again = JsonlStorage::open(b.join("moved/sessions/s.jsonl")).unwrap();
    assert_eq!(again.entries().len(), 3);
}

#[test]
fn an_opened_session_stays_put_when_its_directory_is_swapped_for_a_link() {
    let b = base("open");
    let sessions = b.join(".tole").join("sessions");
    std::fs::create_dir_all(&sessions).unwrap();
    {
        let mut s = JsonlStorage::create(&sessions, "s", None).unwrap();
        s.commit(Commit::new().entry(msg(1))).unwrap();
    }
    let mut s = JsonlStorage::open(sessions.join("s.jsonl")).unwrap();
    let elsewhere = swap(&b);
    s.commit(Commit::new().entry(msg(2))).unwrap();
    s.compact().unwrap();
    assert!(names(&elsewhere.join("sessions")).is_empty());
    assert!(b.join("moved/sessions/s.jsonl").is_file());
    drop(s);
    assert_eq!(
        JsonlStorage::open(b.join("moved/sessions/s.jsonl"))
            .unwrap()
            .entries()
            .len(),
        2
    );
}

#[test]
fn the_lock_sidecar_still_refuses_a_second_writer() {
    let b = base("lock");
    let sessions = b.join("sessions");
    std::fs::create_dir_all(&sessions).unwrap();
    let _held = JsonlStorage::create(&sessions, "s", None).unwrap();
    let err = JsonlStorage::open(sessions.join("s.jsonl")).err().unwrap();
    assert!(
        matches!(err, tole_core::storage::StorageError::Locked { .. }),
        "{err:?}"
    );
}
