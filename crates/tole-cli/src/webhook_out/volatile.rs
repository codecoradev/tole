//! Approval events (#911, design sections 4 and 8): a bounded, in-memory queue
//! and the notifier that fills it.
//!
//! Approval events are **volatile**: `ApprovalQueue` itself is in memory and
//! empty after a restart, so replaying `approval_requested` would announce
//! something that can no longer be decided. A gateway reconciles with
//! `GET /approvals` when it starts. The queue holds at most 256 events per
//! destination; a new one is dropped (and counted) when it is full.
//!
//! Nothing here does I/O. [`notifier`] returns the callback the approval queue
//! calls, after its own locks are released, on the thread that caused the
//! event: a turn queuing a request, the decision route, the expiry timer. It
//! only builds the (redacted) body and pushes it, holding this queue's mutex
//! for the push alone, so an approval is never delayed by a slow receiver.

use std::collections::{BTreeMap, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use tole_core::secrets::SecretSet;

use crate::approvals::{ApprovalEvent, ApprovalNotifier, ApprovalStatus};

use super::event::{self, ApprovalOutcome, ApprovalRequest, Resolution};
use super::{Destinations, Event};

/// Events held per destination.
pub const MAX_PER_DESTINATION: usize = 256;
/// How long an approval event stays worth sending: an approval lives 900 s, and
/// a stale `approval_requested` is worse than none.
pub const LIFETIME_SECS: i64 = crate::approvals::EXPIRY_SECS as i64;

/// One event waiting to be sent.
#[derive(Clone, PartialEq, Eq)]
pub struct Queued {
    pub dest: String,
    pub id: String,
    pub event: Event,
    /// The exact body that is signed and sent, redacted when it was built.
    pub body: String,
    /// Not sent after this time (unix seconds).
    pub deadline: i64,
    /// Attempt results so far.
    pub attempts: u32,
    /// Not before this time.
    pub next_at: i64,
}

// The body is redacted but private: `Debug` shows the shape only.
impl std::fmt::Debug for Queued {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Queued")
            .field("dest", &self.dest)
            .field("id", &self.id)
            .field("event", &self.event)
            .field("attempts", &self.attempts)
            .finish_non_exhaustive()
    }
}

/// Why an event was not queued.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refused {
    /// 256 events are already waiting for this destination.
    Full,
}

/// The bounded queue. One consumer (the sender) and any number of producers.
#[derive(Default)]
pub struct VolatileQueue {
    by_dest: Mutex<BTreeMap<String, VecDeque<Queued>>>,
    dropped: AtomicU64,
}

impl VolatileQueue {
    pub fn new() -> Arc<VolatileQueue> {
        Arc::new(VolatileQueue::default())
    }

    /// Appends `event`, unless 256 are waiting for its destination (then it is
    /// dropped and counted). The mutex is held for the push only.
    pub fn push(&self, event: Queued) -> Result<(), Refused> {
        let mut m = self.by_dest.lock().unwrap_or_else(|p| p.into_inner());
        let q = m.entry(event.dest.clone()).or_default();
        if q.len() >= MAX_PER_DESTINATION {
            drop(m);
            self.dropped.fetch_add(1, Ordering::Relaxed);
            return Err(Refused::Full);
        }
        q.push_back(event);
        Ok(())
    }

    /// Events dropped because a destination's queue was full, since start.
    pub fn dropped(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }

    /// Events waiting for `dest`.
    pub fn len(&self, dest: &str) -> usize {
        self.by_dest
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .get(dest)
            .map_or(0, VecDeque::len)
    }

    pub fn is_empty(&self) -> bool {
        self.by_dest
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .values()
            .all(VecDeque::is_empty)
    }

    /// Removes and returns, for each destination, the oldest event that is due
    /// at `now` (its retry time has come). FIFO per destination, but an event
    /// that is waiting for a retry does not hold back the ones behind it, so one
    /// poisoned event cannot stall a destination. Events past their deadline are
    /// removed too and returned in `expired`: they are never sent.
    pub fn take_due(&self, now: i64) -> Taken {
        let mut m = self.by_dest.lock().unwrap_or_else(|p| p.into_inner());
        let mut out = Taken::default();
        for q in m.values_mut() {
            let mut i = 0;
            while i < q.len() {
                if q[i].deadline <= now {
                    out.expired.extend(q.remove(i));
                } else {
                    i += 1;
                }
            }
            if let Some(pos) = q.iter().position(|e| e.next_at <= now) {
                out.due.extend(q.remove(pos));
            }
        }
        m.retain(|_, q| !q.is_empty());
        out
    }

    /// Puts an event back (a retry). It goes to the back; only its own
    /// `next_at` holds it, and it still counts against the bound.
    pub fn requeue(&self, event: Queued) -> Result<(), Refused> {
        self.push(event)
    }
}

/// What [`VolatileQueue::take_due`] removed.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Taken {
    /// One due event per destination, to be sent now.
    pub due: Vec<Queued>,
    /// Events whose deadline passed before they could be sent.
    pub expired: Vec<Queued>,
}

fn resolution(status: ApprovalStatus) -> Option<Resolution> {
    match status {
        ApprovalStatus::Approved => Some(Resolution::Approved),
        ApprovalStatus::Denied => Some(Resolution::Denied),
        ApprovalStatus::Expired => Some(Resolution::Expired),
        ApprovalStatus::Pending => None,
    }
}

/// Builds the body of `event` for each destination that lists its type, with
/// every text field redacted by `redact`, and returns what to queue. A
/// destination that does not list the type gets nothing, and neither does an
/// approval whose id is not a valid delivery id.
pub fn build(
    dests: &Destinations,
    event: &ApprovalEvent,
    now: i64,
    redact: &dyn Fn(&str) -> String,
) -> Vec<Queued> {
    let kind = match event {
        ApprovalEvent::Requested(_) => Event::ApprovalRequested,
        ApprovalEvent::Resolved { .. } => Event::ApprovalResolved,
        // A new variant is not announced until it is handled here.
        #[allow(unreachable_patterns)]
        _ => return Vec::new(),
    };
    let mut out = Vec::new();
    for name in dests.names() {
        let Some(d) = dests.get(name) else { continue };
        if !d.events.contains(&kind) {
            continue;
        }
        let built = match event {
            ApprovalEvent::Requested(i) => event::approval_requested(
                name,
                now,
                &ApprovalRequest {
                    approval_id: &i.id,
                    session_id: &i.session_id,
                    tool: &i.tool,
                    description: &i.description,
                    created_at: (i.created_at_ms / 1000) as i64,
                    expires_at: (i.created_at_ms / 1000) as i64 + LIFETIME_SECS,
                },
                redact,
            ),
            ApprovalEvent::Resolved { info, status } => {
                let Some(r) = resolution(*status) else {
                    continue;
                };
                event::approval_resolved(
                    name,
                    now,
                    &ApprovalOutcome {
                        approval_id: &info.id,
                        session_id: &info.session_id,
                        tool: &info.tool,
                        resolution: r,
                        resolved_at: now,
                    },
                    redact,
                )
            }
            #[allow(unreachable_patterns)]
            _ => continue,
        };
        let Ok(built) = built else { continue };
        let Ok(body) = String::from_utf8(built.body) else {
            continue;
        };
        out.push(Queued {
            dest: name.to_string(),
            id: built.id,
            event: kind,
            body,
            // A request is worth sending until it would have expired; a
            // resolution for as long as an approval lives.
            deadline: match event {
                ApprovalEvent::Requested(i) => (i.created_at_ms / 1000) as i64 + LIFETIME_SECS,
                _ => now + LIFETIME_SECS,
            },
            attempts: 0,
            next_at: 0,
        });
    }
    out
}

/// The callback to install with `ApprovalQueue::set_notifier`: builds the events
/// for the destinations that list them and pushes them onto `queue`. `clock`
/// and `secrets` are read per event (the secret set grows as role secrets are
/// registered). It does no I/O and never blocks beyond the queue's own mutex.
pub fn notifier(
    dests: Arc<Destinations>,
    queue: Arc<VolatileQueue>,
    clock: Arc<dyn Fn() -> i64 + Send + Sync>,
    secrets: Arc<dyn Fn() -> SecretSet + Send + Sync>,
) -> ApprovalNotifier {
    Arc::new(move |event| {
        let set = secrets();
        let redact = |s: &str| set.redact(s).into_owned();
        for q in build(&dests, &event, clock(), &redact) {
            // A full queue drops the new event and counts it.
            let _ = queue.push(q);
        }
    })
}

#[cfg(test)]
#[path = "volatile_tests.rs"]
mod tests;
