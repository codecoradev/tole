//! The wake-up between the backend's threads and the window's own thread.
//!
//! The backend calls `on_change` on its worker thread, with no lock held, and
//! the launch thread ends on a thread of its own. None of them may touch the
//! window or wait for it, so they do the one thing that cannot block: raise
//! this flag. The window's thread asks [`Signal::take`] (cheap, lock-free) on
//! its own timer and, when it says `true`, reads the view and repaints. A
//! hundred raises between two looks are one change, which is all the window
//! needs: it reads the whole view afresh.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

/// A shared "something changed" flag. Clones share the flag.
#[derive(Clone, Debug, Default)]
pub struct Signal(Arc<AtomicBool>);

impl Signal {
    pub fn new() -> Self {
        Signal::default()
    }

    /// Something changed. Never blocks; safe from any thread.
    pub fn raise(&self) {
        self.0.store(true, Ordering::SeqCst);
    }

    /// Whether something changed since the last look; clears it.
    pub fn take(&self) -> bool {
        self.0.swap(false, Ordering::SeqCst)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_raise_is_seen_once_and_many_raises_are_one() {
        let signal = Signal::new();
        assert!(!signal.take(), "nothing raised yet");
        for _ in 0..100 {
            signal.raise();
        }
        assert!(signal.take());
        assert!(!signal.take(), "taken: clear until the next raise");
    }

    #[test]
    fn a_clone_on_another_thread_reaches_the_owner() {
        let signal = Signal::new();
        let there = signal.clone();
        std::thread::spawn(move || there.raise()).join().unwrap();
        assert!(signal.take());
    }
}
