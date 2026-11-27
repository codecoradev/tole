//! The cancel token: visibility on clones and across threads, detach versus cancel.
//! Moved out of `src/` (#1249): these tests only use the public API of the crate.

use tole_core::cancel::CancelToken;

#[test]
fn default_token_is_never_cancelled() {
    let t = CancelToken::default();
    assert!(!t.is_cancelled());
}

#[test]
fn cancel_is_visible_on_clones() {
    let t = CancelToken::new();
    let c1 = t.clone();
    let c2 = t.clone();
    assert!(!c1.is_cancelled());
    t.cancel();
    assert!(c1.is_cancelled());
    assert!(c2.is_cancelled());
    // Idempotent: cancelling twice stays cancelled, no panic.
    c2.cancel();
    assert!(t.is_cancelled());
}

#[test]
fn cancel_from_another_thread_is_observed() {
    let t = CancelToken::new();
    let handle_t = t.clone();
    let h = std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(20));
        handle_t.cancel();
    });
    // A spin-wait (no sleep on this side) proves cross-thread
    // visibility without timing assumptions beyond the spawned
    // cancel landing.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while !t.is_cancelled() && std::time::Instant::now() < deadline {
        std::thread::yield_now();
    }
    h.join().unwrap();
    assert!(t.is_cancelled());
}

#[test]
fn detach_is_separate_from_cancel_and_reset_clears_it() {
    let t = CancelToken::new();
    let c = t.clone();
    assert!(!c.is_detached());
    t.detach();
    assert!(c.is_detached() && !c.is_cancelled());
    t.reset();
    assert!(!c.is_detached());
}
