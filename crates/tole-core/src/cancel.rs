//! Cooperative cancellation for server-face turns (issue #178).
//!
//! ACP clients MUST be able to stop a generating turn (`session/cancel`
//! is a protocol baseline MUST). The turn loop is synchronous, so the
//! token is a checkpoint flag: it is checked between steps and before
//! each tool execution — never mid-step. A set token unwinds the turn
//! into a durable `TurnOutcome::Cancelled` settlement (pc = Final, the
//! session stays prompt-resumable) and the transport answers the
//! original prompt request with `stopReason: "cancelled"`.
//!
//! `serve` shares `run_session_turn` but has no cancel endpoint today;
//! there the token is simply never set (default-constructed = never
//! cancelled), so REST behavior is unchanged.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

/// Shared, cheap, cooperative cancellation checkpoint. `Clone` keeps
/// all handles observing the same flag; `Default` yields a token that
/// is never cancelled (serve's shape).
#[derive(Clone, Default)]
pub struct CancelToken {
    flag: Arc<AtomicBool>,
}

impl CancelToken {
    pub fn new() -> Self {
        Self::default()
    }

    /// Set by the transport when the client cancels the session's
    /// current turn. Idempotent; safe from any thread.
    pub fn cancel(&self) {
        self.flag.store(true, Ordering::SeqCst);
    }

    /// Checkpoint test. `SeqCst` keeps the store/loads ordered with the
    /// surrounding durable commits — a cancelled turn must never commit
    /// a step AFTER observing the flag.
    pub fn is_cancelled(&self) -> bool {
        self.flag.load(Ordering::SeqCst)
    }

    /// Clear the flag. Called when a session claims `busy` for a NEW
    /// turn (inside the same critical section): a cancel can only be
    /// lost if it arrives before the turn it targets was accepted —
    /// at which point no turn was in flight and wiping is correct.
    pub fn reset(&self) {
        self.flag.store(false, Ordering::SeqCst);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
