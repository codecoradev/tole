//! What only the module itself can see: the shape check that spares the file, and the time format.

use super::cmd::utc;
use super::*;
use crate::test_dir::TestDir;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

#[test]
fn a_token_of_the_wrong_shape_costs_no_file_access() {
    let d = TestDir::new("tole-dt-shape");
    let s = Store::new(&d.join("tole"));
    let live = Live::new(s.clone());
    s.add("phone", Scope::Full, 1).unwrap();
    assert_eq!(live.lookup("the-shared-bearer-token"), None);
    assert_eq!(live.lookup(PREFIX), None);
    assert!(!live.loaded(), "the file was never looked at");
    // A token with the shape does read it.
    assert_eq!(live.lookup(&format!("{PREFIX}{}", "A".repeat(43))), None);
    assert!(live.loaded());
}

#[test]
fn times_are_utc_minutes() {
    assert_eq!(utc(0), "1970-01-01 00:00Z");
    assert_eq!(utc(1_795_000_000), "2026-11-18 11:06Z");
}

/// A view over a store with one token, whose loads are counted and can be
/// made to fail with the error `fail` makes.
fn counted(
    tag: &str,
    retry: Duration,
    fail: impl Fn(usize) -> Option<StoreError> + Send + Sync + 'static,
) -> (TestDir, Live, Arc<AtomicUsize>, String) {
    let d = TestDir::new(&format!("tole-dt-{tag}"));
    let s = Store::new(&d.join("tole"));
    let secret = s
        .add("phone", Scope::Full, 1)
        .unwrap()
        .secret
        .expose()
        .to_string();
    let loads = Arc::new(AtomicUsize::new(0));
    let (n, s2) = (loads.clone(), s.clone());
    let loader: super::store::Loader = Box::new(move || {
        let i = n.fetch_add(1, Ordering::SeqCst);
        match fail(i) {
            Some(e) => Err(e),
            None => s2.load(),
        }
    });
    (d, Live::with_loader(s, loader, retry), loads, secret)
}

fn io() -> StoreError {
    StoreError::Io(std::io::Error::other("transient"))
}

#[test]
fn an_io_error_is_not_sticky_it_is_retried() {
    // Fails twice, then works; the file never changes.
    let (_d, live, loads, t) = counted("io-retry", Duration::ZERO, |i| (i < 2).then(io));
    assert_eq!(live.lookup(&t), None);
    assert_eq!(live.lookup(&t), None);
    assert_eq!(live.lookup(&t).unwrap().name, "phone");
    assert_eq!(loads.load(Ordering::SeqCst), 3);
    // A good load is cached by the stamp again.
    assert!(live.lookup(&t).is_some());
    assert_eq!(loads.load(Ordering::SeqCst), 3);
}

#[test]
fn an_io_error_waits_out_its_retry_time_and_is_not_reread_per_request() {
    let (_d, live, loads, t) = counted("io-wait", Duration::from_secs(3600), |_| Some(io()));
    for _ in 0..50 {
        assert_eq!(live.lookup(&t), None);
    }
    assert_eq!(
        loads.load(Ordering::SeqCst),
        1,
        "no read per request in the meantime"
    );
}

#[test]
fn an_io_error_is_also_dropped_when_the_file_changes() {
    let (d, live, loads, t) = counted("io-stamp", Duration::from_secs(3600), |i| (i == 0).then(io));
    assert_eq!(live.lookup(&t), None);
    assert_eq!(live.lookup(&t), None, "still waiting for the retry time");
    // An append changes the stamp: no need to wait for it.
    Store::new(&d.join("tole"))
        .add("tablet", Scope::Full, 2)
        .unwrap();
    assert!(live.lookup(&t).is_some());
    assert_eq!(loads.load(Ordering::SeqCst), 2);
}

#[test]
fn a_failure_that_belongs_to_the_file_stays_until_the_file_changes() {
    for (what, e) in [
        (
            "corrupt",
            (|| StoreError::Corrupt { line: 1, why: "x" }) as fn() -> StoreError,
        ),
        ("too large", || StoreError::TooLarge),
        ("unsafe", || StoreError::Unsafe("mode".into())),
    ] {
        let (_d, live, loads, t) = counted(what, Duration::ZERO, move |_| Some(e()));
        for _ in 0..20 {
            assert_eq!(live.lookup(&t), None);
        }
        assert_eq!(
            loads.load(Ordering::SeqCst),
            1,
            "{what}: cached, never re-read"
        );
    }
}

#[cfg(unix)]
#[test]
fn the_stamp_has_the_inode_and_a_replaced_file_is_noticed() {
    let d = TestDir::new("tole-dt-inode");
    let s = Store::new(&d.join("tole"));
    s.add("phone", Scope::Full, 1).unwrap();
    let live = Live::new(s.clone());
    let file = d.join("tole/serve-tokens.jsonl");
    let before = live.stamp().unwrap();
    // A copy with the same bytes and the same mtime, renamed over the file.
    let copy = d.join("tole/copy");
    std::fs::copy(&file, &copy).unwrap();
    let mtime = std::fs::metadata(&file).unwrap().modified().unwrap();
    std::fs::File::options()
        .write(true)
        .open(&copy)
        .unwrap()
        .set_modified(mtime)
        .unwrap();
    std::fs::rename(&copy, &file).unwrap();
    let after = live.stamp().unwrap();
    assert_eq!((before.len, before.mtime), (after.len, after.mtime));
    assert_ne!(before.ino, after.ino);
    assert_ne!(before, after);
}
