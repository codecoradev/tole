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

/// #552: `flock` belongs to the open file description, so a child forked by
/// ANOTHER thread briefly holds a duplicate of our lock descriptor until it
/// execs (CLOEXEC only closes it AT exec). A reopen right after `drop` could
/// then see `Locked`, with our own pid as the holder. The reopen must ride
/// that out.
#[test]
fn a_reopen_after_drop_survives_children_forked_by_other_threads() {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;

    let dir = tmpdir("fork-race");
    let stop = Arc::new(AtomicBool::new(false));
    let spawners: Vec<_> = (0..3)
        .map(|_| {
            let stop = stop.clone();
            std::thread::spawn(move || {
                while !stop.load(Ordering::Relaxed) {
                    let _ = Command::new("true").status();
                }
            })
        })
        .collect();

    let mut failures = Vec::new();
    for i in 0..6000 {
        let id = format!("race-{}", i % 8);
        let path = dir.join(format!("{id}.jsonl"));
        let s = if path.exists() {
            JsonlStorage::open(&path)
        } else {
            JsonlStorage::create(&dir, &id, None)
        };
        match s {
            Ok(s) => drop(s),
            Err(e) => failures.push(format!("round {i}: {e:?}")),
        }
    }
    stop.store(true, Ordering::Relaxed);
    for t in spawners {
        t.join().unwrap();
    }
    assert!(
        failures.is_empty(),
        "{} of 6000 reopens failed, first: {:?}",
        failures.len(),
        failures.first()
    );
}

/// Stands in for a forked child's duplicate of a closed lock descriptor:
/// a thread takes the sidecar lock of session `id`, writes our own pid in
/// it, and holds it for `ms` milliseconds. Returns once the lock is held.
fn hold_as_duplicate(dir: &std::path::Path, id: &str, ms: u64) -> std::thread::JoinHandle<()> {
    drop(JsonlStorage::create(dir, id, None).unwrap());
    let lock_path = dir.join(format!("{id}.jsonl.lock"));
    let (tx, rx) = std::sync::mpsc::channel();
    let holder = std::thread::spawn(move || {
        let mut f = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&lock_path)
            .unwrap();
        // Children forked by other tests of this binary can still hold a
        // duplicate of the lock `create` just released: wait for it.
        while f.try_lock().is_err() {
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        f.set_len(0).unwrap();
        write!(f, "{}", std::process::id()).unwrap();
        tx.send(()).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(ms));
    });
    rx.recv().unwrap();
    holder
}

/// #712: a forked child that has not exec'd yet can keep a duplicate of the
/// closed lock descriptor for much longer than 150 ms on a starved machine
/// (one CPU, heavy load). The hold names our own pid but no handle of this
/// process owns it, so the reopen waits it out. The duplicate is simulated
/// by a thread holding the sidecar lock, with our pid in it, for 700 ms.
#[test]
fn a_reopen_waits_out_a_long_lived_duplicate_of_a_closed_lock() {
    let dir = tmpdir("long-dup");
    let holder = hold_as_duplicate(&dir, "d", 700);
    let s = JsonlStorage::open(dir.join("d.jsonl"));
    assert!(s.is_ok(), "{:?}", s.err());
    holder.join().unwrap();
}

/// The retry must not blur the two real refusals: another process holding
/// the session fails at once, and a second open in THIS process (#475) still
/// fails, after at most the bounded retry.
#[test]
fn real_second_opens_are_still_refused_within_the_bound() {
    let dir = tmpdir("bound");
    let mut child = spawn_holder(&dir, "plain");
    let path = dir.join("held.jsonl");
    let t = std::time::Instant::now();
    let err = open_err(&path);
    assert!(matches!(err, StorageError::Locked { .. }), "{err:?}");
    assert!(
        t.elapsed() < std::time::Duration::from_millis(50),
        "a holder with another pid fails immediately, took {:?}",
        t.elapsed()
    );
    child.kill().unwrap();
    child.wait().unwrap();

    let own = JsonlStorage::create(&dir, "own", None).unwrap();
    let t = std::time::Instant::now();
    let err = open_err(&dir.join("own.jsonl"));
    assert!(
        matches!(err, StorageError::Locked { holder: Some(p) } if p == std::process::id()),
        "{err:?}"
    );
    assert!(
        t.elapsed() < std::time::Duration::from_millis(1000),
        "the same-process refusal is bounded, took {:?}",
        t.elapsed()
    );
    drop(own);
}

/// The retry reads the lock file between attempts, which moves its cursor.
/// The pid written once the lock IS taken must still land at offset 0, or
/// the file is NUL-padded and the next refusal cannot name its holder.
#[test]
fn the_pid_is_written_cleanly_after_a_retried_acquisition() {
    let dir = tmpdir("retry-pid");
    let releaser = hold_as_duplicate(&dir, "r", 40);
    // Blocks (retrying: the holder is our own pid, but no handle of ours)
    // until the duplicate is released, then takes the lock.
    let reopened = JsonlStorage::open(dir.join("r.jsonl")).expect("retry rides out the release");
    releaser.join().unwrap();
    let text = std::fs::read(dir.join("r.jsonl.lock")).unwrap();
    assert_eq!(
        text,
        std::process::id().to_string().into_bytes(),
        "the lock file holds exactly our pid, no padding"
    );
    // And a real refusal now still names us.
    let err = open_err(&dir.join("r.jsonl"));
    assert!(
        matches!(err, StorageError::Locked { holder: Some(p) } if p == std::process::id()),
        "{err:?}"
    );
    drop(reopened);
}
