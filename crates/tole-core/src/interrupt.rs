//! Soft interrupts: steer a running turn without cancelling it (#377).
//!
//! The owner (a second client of a `serve`/MCP session) pushes a message
//! while a turn is running. The turn loop drains the queue at its safe
//! point — after the current tool has settled and before the next provider
//! call — and commits each message as an attributed user-role entry, so
//! nothing done so far is lost (unlike a cancel).
//!
//! `urgent` additionally withdraws a tool call the model has just asked for
//! but that has not started: the call is dropped BEFORE its intent is
//! written, so no `tool_use` is ever left without a `tool_result`, and a
//! durable error entry records the drop (no call vanishes silently).
//!
//! The queue rides inside [`crate::cancel::CancelToken`], the per-session
//! turn handle every host already shares; a default token has an empty
//! queue nothing pushes to, so existing callers are unchanged.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

/// Most interrupts that may wait for a safe point.
pub const MAX_PENDING: usize = 16;
/// Largest accepted interrupt text, in bytes.
pub const MAX_TEXT_BYTES: usize = 8 * 1024;

/// Who queued an interrupt: the owner steering the turn, or the host
/// relaying an informational notice (e.g. a sibling agent changed a file,
/// #380). A notice never withdraws output and never supersedes a final
/// answer — it is delivered at the next safe point if the turn has one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    Owner,
    Notice,
}

/// One message injected mid-turn.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Interrupt {
    pub text: String,
    /// Also withdraw a tool call that has not started yet (owner only).
    pub urgent: bool,
    pub source: Source,
}

/// Why a push was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PushError {
    Empty,
    TooLong,
    Full,
}

impl std::fmt::Display for PushError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PushError::Empty => f.write_str("interrupt text is empty"),
            PushError::TooLong => write!(f, "interrupt text exceeds {MAX_TEXT_BYTES} bytes"),
            PushError::Full => write!(f, "{MAX_PENDING} interrupts are already pending"),
        }
    }
}

/// Shared, bounded queue of pending interrupts. `Clone` shares the queue.
#[derive(Clone, Default)]
pub struct InterruptQueue {
    q: Arc<Mutex<VecDeque<Interrupt>>>,
}

impl InterruptQueue {
    fn lock(&self) -> std::sync::MutexGuard<'_, VecDeque<Interrupt>> {
        // A panicking pusher must not wedge steering for the session.
        self.q.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Queue a message. Bounded: blank, oversized, or beyond
    /// [`MAX_PENDING`] pending are refused, never truncated.
    pub fn push(&self, text: &str, urgent: bool) -> Result<(), PushError> {
        self.push_from(text, urgent, Source::Owner)
    }

    /// Queue an informational notice (never urgent). Same bounds as
    /// [`InterruptQueue::push`].
    pub fn push_notice(&self, text: &str) -> Result<(), PushError> {
        self.push_from(text, false, Source::Notice)
    }

    fn push_from(&self, text: &str, urgent: bool, source: Source) -> Result<(), PushError> {
        let text = text.trim();
        if text.is_empty() {
            return Err(PushError::Empty);
        }
        if text.len() > MAX_TEXT_BYTES {
            return Err(PushError::TooLong);
        }
        let mut q = self.lock();
        if q.len() >= MAX_PENDING {
            return Err(PushError::Full);
        }
        q.push_back(Interrupt {
            text: text.to_string(),
            urgent,
            source,
        });
        Ok(())
    }

    /// Take everything pending, oldest first.
    pub fn drain(&self) -> Vec<Interrupt> {
        self.lock().drain(..).collect()
    }

    pub fn is_empty(&self) -> bool {
        self.lock().is_empty()
    }

    /// Is an owner message waiting (a notice does not count)?
    pub fn has_owner(&self) -> bool {
        self.lock().iter().any(|i| i.source == Source::Owner)
    }

    /// Is an urgent interrupt waiting?
    pub fn has_urgent(&self) -> bool {
        self.lock().iter().any(|i| i.urgent)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn drains_in_order_and_empties() {
        let q = InterruptQueue::default();
        q.push("first", false).unwrap();
        q.push("  second  ", true).unwrap();
        assert!(q.has_urgent());
        let d = q.drain();
        assert_eq!(
            d,
            vec![
                Interrupt {
                    text: "first".into(),
                    urgent: false,
                    source: Source::Owner,
                },
                Interrupt {
                    text: "second".into(),
                    urgent: true,
                    source: Source::Owner,
                },
            ]
        );
        assert!(q.is_empty() && !q.has_urgent());
        assert!(q.drain().is_empty());
    }

    #[test]
    fn a_notice_is_never_urgent_and_is_not_an_owner_message() {
        let q = InterruptQueue::default();
        q.push_notice("sibling changed a.rs").unwrap();
        assert!(!q.has_urgent() && !q.has_owner() && !q.is_empty());
        q.push("steer", false).unwrap();
        assert!(q.has_owner());
        let d = q.drain();
        assert_eq!(d[0].source, Source::Notice);
        assert!(!d[0].urgent);
        assert_eq!(d[1].source, Source::Owner);
    }

    #[test]
    fn clones_share_the_queue() {
        let a = InterruptQueue::default();
        let b = a.clone();
        a.push("x", false).unwrap();
        assert_eq!(b.drain().len(), 1);
    }

    #[test]
    fn push_is_bounded_never_truncated() {
        let q = InterruptQueue::default();
        assert_eq!(q.push("   ", false), Err(PushError::Empty));
        assert_eq!(
            q.push(&"x".repeat(MAX_TEXT_BYTES + 1), false),
            Err(PushError::TooLong)
        );
        for _ in 0..MAX_PENDING {
            q.push("m", false).unwrap();
        }
        assert_eq!(q.push("m", false), Err(PushError::Full));
        assert_eq!(q.drain().len(), MAX_PENDING);
    }
}
