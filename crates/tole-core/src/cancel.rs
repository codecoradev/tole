//! Cooperative cancellation for server-face turns (issue #178).
//!
//! ACP clients MUST be able to stop a generating turn (`session/cancel`
//! is a protocol baseline MUST). The turn loop is synchronous, so the
//! token is a checkpoint flag: it is checked between steps and before
//! each tool execution. A provider call already in flight is interrupted
//! by `Provider::complete_with_cancel` (#484; the abandoned request may
//! still be billed). A set token unwinds the turn
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
    /// Detach request (#217): stop waiting, keep the work. Separate from
    /// `flag` so a cancel (abort) always outranks it.
    detached: Arc<AtomicBool>,
    /// Soft-interrupt queue (#377): the same per-session turn handle that
    /// carries the cancel flag also carries steering messages. Never
    /// cleared by `reset`: an interrupt waits for the next safe point.
    interrupts: crate::interrupt::InterruptQueue,
}

impl CancelToken {
    pub fn new() -> Self {
        Self::default()
    }

    /// The session's soft-interrupt queue (#377).
    pub fn interrupts(&self) -> &crate::interrupt::InterruptQueue {
        &self.interrupts
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
        self.detached.store(false, Ordering::SeqCst);
    }

    /// Ask the running turn to stop waiting WITHOUT aborting anything
    /// (#217): the in-flight call settles normally, then the turn ends
    /// `Detached` (pc = Final, resumable). Idempotent; any thread.
    pub fn detach(&self) {
        self.detached.store(true, Ordering::SeqCst);
    }

    pub fn is_detached(&self) -> bool {
        self.detached.load(Ordering::SeqCst)
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
}
