//! Cross-process writer exclusion (issue #475): a writable `JsonlStorage`
//! holds an advisory lock on a sidecar `<id>.jsonl.lock` for its whole
//! lifetime; a second writer is refused instead of corrupting the file.

use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};

use serde_json::json;

use tole_core::entry::{EntryType, NewEntry};
use tole_core::storage::{Commit, JsonlStorage, Storage, StorageError};

const CHILD_ENV: &str = "TOLE_LOCK_CHILD";
const DIR_ENV: &str = "TOLE_LOCK_DIR";
const TEST_NAME: &str = "child_holder";

fn tmpdir(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("tole-lock-{}-{}", tag, std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn msg(text: &str) -> Commit {
    Commit::new().entry(NewEntry::root(
        EntryType::new(EntryType::MESSAGE),
        json!({ "role": "user", "text": text }),
    ))
}

/// Child half: create a session, optionally compact it, print `ready`,
/// then hold the writer until stdin closes (or the parent kills us).
#[test]
fn child_holder() {
    let Some(mode) = std::env::var_os(CHILD_ENV) else {
        return; // plain run of the suite: nothing to do
    };
    let dir = PathBuf::from(std::env::var(DIR_ENV).unwrap());
    let mut s = JsonlStorage::create(&dir, "held", None).unwrap();
    s.commit(msg("from child")).unwrap();
    if mode == "compact" {
        s.compact().unwrap();
    }
    println!("ready");
    std::io::stdout().flush().unwrap();
    let mut sink = String::new();
    let _ = std::io::stdin().read_line(&mut sink);
    drop(s);
}

fn spawn_holder(dir: &PathBuf, mode: &str) -> Child {
    let mut child = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", TEST_NAME, "--nocapture"])
        .env(CHILD_ENV, mode)
        .env(DIR_ENV, dir)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut out = BufReader::new(child.stdout.take().unwrap());
    let mut line = String::new();
    loop {
        line.clear();
        assert!(out.read_line(&mut line).unwrap() > 0, "child died early");
        if line.trim() == "ready" {
            break;
        }
    }
    child
}

fn open_err(path: &PathBuf) -> StorageError {
    match JsonlStorage::open(path) {
        Ok(_) => panic!("second writer must be refused"),
        Err(e) => e,
    }
}

#[test]
fn second_process_writer_is_refused_naming_the_holder() {
    let dir = tmpdir("refuse");
    let mut child = spawn_holder(&dir, "plain");
    let path = dir.join("held.jsonl");
    let before = std::fs::read(&path).unwrap();

    let err = open_err(&path);
    assert!(matches!(err, StorageError::Locked { .. }), "{err:?}");
    assert!(
        err.to_string().contains(&format!("pid {}", child.id())),
        "error must name the holder: {err}"
    );
    assert_eq!(std::fs::read(&path).unwrap(), before, "file untouched");

    // Read-only access stays lock-free.
    let ro = JsonlStorage::open_read_only(&path).unwrap();
    assert_eq!(ro.last_seq(), 1);

    // SIGKILL releases the OS lock; the session opens and appends cleanly.
    child.kill().unwrap();
    child.wait().unwrap();
    let mut s = JsonlStorage::open(&path).unwrap();
    s.commit(msg("after crash")).unwrap();
    drop(s);
    assert_eq!(JsonlStorage::open_read_only(&path).unwrap().last_seq(), 2);
}

#[test]
fn lock_survives_compact_rename() {
    let dir = tmpdir("compact");
    let mut child = spawn_holder(&dir, "compact");
    let err = open_err(&dir.join("held.jsonl"));
    assert!(matches!(err, StorageError::Locked { .. }), "{err:?}");
    child.kill().unwrap();
    child.wait().unwrap();
}

#[test]
fn torn_tail_is_not_repaired_without_the_lock() {
    let dir = tmpdir("torn");
    let mut a = JsonlStorage::create(&dir, "t", None).unwrap();
    a.commit(msg("one")).unwrap();
    let path = dir.join("t.jsonl");
    // Another writer is "mid-append": a newline-less fragment on disk.
    let mut f = std::fs::OpenOptions::new()
        .append(true)
        .open(&path)
        .unwrap();
    f.write_all(b"{\"kind\":\"entry\",\"se").unwrap();
    let len = std::fs::metadata(&path).unwrap().len();

    let err = open_err(&path);
    assert!(matches!(err, StorageError::Locked { .. }), "{err:?}");
    assert_eq!(
        std::fs::metadata(&path).unwrap().len(),
        len,
        "refused open must not truncate"
    );
    drop(a);
    // Once the lock is free the repair runs as before.
    JsonlStorage::open(&path).unwrap();
    assert!(std::fs::metadata(&path).unwrap().len() < len);
}

#[test]
fn stale_handle_refuses_to_append_after_foreign_write() {
    let dir = tmpdir("stale");
    let mut a = JsonlStorage::create(&dir, "s", None).unwrap();
    a.commit(msg("one")).unwrap();
    let path = dir.join("s.jsonl");
    // A writer that ignored the lock (older build) appended behind us.
    let mut f = std::fs::OpenOptions::new()
        .append(true)
        .open(&path)
        .unwrap();
    f.write_all(b"{\"kind\":\"usage\",\"seq\":2,\"id\":\"u\",\"entryId\":\"e_1\",\"usage\":{\"promptTokens\":1,\"completionTokens\":1,\"totalTokens\":2}}\n").unwrap();
    let len = std::fs::metadata(&path).unwrap().len();

    let err = a.commit(msg("two")).unwrap_err();
    assert!(matches!(err, StorageError::StaleHandle(_)), "{err:?}");
    assert_eq!(
        std::fs::metadata(&path).unwrap().len(),
        len,
        "nothing appended"
    );
}
