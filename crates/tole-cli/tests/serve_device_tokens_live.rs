//! Per-device tokens (#1320): the `Live` view `tole serve` holds (revoke seen on the next
//! request, fail closed, coalesced last use).

mod common;
use common::TestDir;
use tole_cli::serve_support::device_tokens::*;

const T0: u64 = 1_795_000_000;

fn setup(tag: &str) -> (TestDir, Store) {
    let d = TestDir::new(&format!("tole-dt-live-{tag}"));
    let s = Store::new(&d.join("tole"));
    (d, s)
}

fn path(d: &TestDir) -> std::path::PathBuf {
    d.join("tole/serve-tokens.jsonl")
}

fn lines(d: &TestDir) -> usize {
    std::fs::read_to_string(path(d)).unwrap().lines().count()
}

#[test]
fn live_sees_an_add_and_a_revoke_on_the_next_request() {
    let (_d, s) = setup("revoke");
    let live = Live::new(s.clone());
    // No file yet: no device token, no error.
    assert_eq!(live.lookup(&format!("{PREFIX}{}", "A".repeat(43))), None);
    assert!(live.error().is_none());
    let a = s.add("phone", Scope::ReadOnly, T0).unwrap();
    let t = a.secret.expose();
    assert_eq!(live.lookup(t).unwrap().name, "phone");
    s.revoke("phone", T0 + 1).unwrap();
    assert_eq!(live.lookup(t), None, "a revoked token is refused at once");
    assert!(live.error().is_none());
}

#[test]
fn live_fails_closed_on_a_broken_file_and_recovers_when_it_is_fixed() {
    let (d, s) = setup("closed");
    let live = Live::new(s.clone());
    let a = s.add("phone", Scope::Full, T0).unwrap();
    let t = a.secret.expose();
    assert!(live.lookup(t).is_some());
    let good = std::fs::read_to_string(path(&d)).unwrap();
    std::fs::write(path(&d), format!("{good}oops\n")).unwrap();
    assert_eq!(live.lookup(t), None);
    assert!(live.error().is_some());
    std::fs::write(path(&d), &good).unwrap();
    assert!(live.lookup(t).is_some());
    assert!(live.error().is_none());
}

#[test]
fn touch_writes_at_most_once_per_interval_and_never_per_request() {
    let (d, s) = setup("touch");
    let live = Live::new(s.clone());
    s.add("phone", Scope::Full, T0).unwrap();
    s.add("laptop", Scope::Full, T0).unwrap();
    assert_eq!(lines(&d), 2);
    assert!(live.touch("phone", T0 + 10));
    for i in 0..1000 {
        assert!(!live.touch("phone", T0 + 10 + i % TOUCH_EVERY_SECS));
    }
    assert_eq!(lines(&d), 3, "a thousand requests, one write");
    assert!(
        live.touch("laptop", T0 + 11),
        "each name has its own interval"
    );
    assert!(live.touch("phone", T0 + 10 + TOUCH_EVERY_SECS));
    let snap = s.load().unwrap();
    assert_eq!(
        snap.active("phone").unwrap().last_used_at,
        Some(T0 + 10 + TOUCH_EVERY_SECS)
    );
    assert_eq!(snap.active("laptop").unwrap().last_used_at, Some(T0 + 11));
    // A use never changes who may get in.
    assert_eq!(snap.entries().iter().filter(|e| e.active()).count(), 2);
}

#[test]
fn touch_never_blocks_on_the_lock_and_skips_a_missing_or_full_file() {
    let (d, s) = setup("busy");
    // No file: nothing to record against.
    assert!(!Live::new(s.clone()).touch("phone", T0));
    s.add("phone", Scope::Full, T0).unwrap();
    {
        // Another process holds the lock (an `add` in progress).
        let held = std::fs::OpenOptions::new()
            .append(true)
            .open(d.join("tole/serve-tokens.lock"))
            .unwrap();
        held.lock().unwrap();
        let before = std::time::Instant::now();
        assert!(!Live::new(s.clone()).touch("phone", T0));
        assert!(before.elapsed() < std::time::Duration::from_secs(5));
    }
    assert!(Live::new(s.clone()).touch("phone", T0));
    // Past the used-lines cap only add and revoke still write.
    use std::io::Write;
    let mut f = std::fs::OpenOptions::new()
        .append(true)
        .open(path(&d))
        .unwrap();
    f.write_all(&vec![b'\n'; MAX_USED_FILE_BYTES as usize])
        .unwrap();
    assert!(!Live::new(s.clone()).touch("phone", T0));
    s.revoke("phone", T0).unwrap();
}

#[test]
fn a_use_of_a_revoked_or_unknown_name_changes_nothing() {
    let (_d, s) = setup("stale");
    let a = s.add("phone", Scope::Full, T0).unwrap();
    s.revoke("phone", T0 + 1).unwrap();
    assert!(Live::new(s.clone()).touch("phone", T0 + 2));
    assert!(Live::new(s.clone()).touch("ghost", T0 + 2));
    let snap = s.load().unwrap();
    assert_eq!(snap.lookup(a.secret.expose()), None);
    assert_eq!(snap.entries()[0].last_used_at, None);
    assert_eq!(snap.entries().len(), 1);
}

#[cfg(unix)]
#[test]
fn live_follows_a_chmod_both_ways_and_reads_a_read_only_file() {
    use std::os::unix::fs::PermissionsExt;
    let (d, s) = setup("chmod");
    let live = Live::new(s.clone());
    let a = s.add("phone", Scope::Full, T0).unwrap();
    let t = a.secret.expose();
    let mode = |m| std::fs::set_permissions(path(&d), std::fs::Permissions::from_mode(m)).unwrap();
    assert!(live.lookup(t).is_some());
    mode(0o644);
    assert_eq!(
        live.lookup(t),
        None,
        "a file others can read is not trusted"
    );
    assert!(live.error().unwrap().contains("not safe"));
    mode(0o400);
    assert!(live.lookup(t).is_some(), "owner-only and read-only is fine");
    assert!(live.error().is_none());
}
