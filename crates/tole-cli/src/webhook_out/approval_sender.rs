//! The sender of approval events (#911, design sections 4 and 8): drains the
//! volatile queue and runs the expiry timer.
//!
//! Started by `tole serve` when a destination lists `approval_requested`,
//! `approval_resolved` or `turn_finished` (#962; the same queue, a longer retry
//! schedule for the turn event, see [`retry_wait`]). It is a fixed set of
//! threads (#980): one timer thread, and one worker per destination that lists
//! one of those events (the destinations are loaded once, so the count is
//! bounded by the config). So a destination that hangs until its timeout (up to
//! 30 s per attempt) only ever delays its own events, never another
//! destination's, and never the timer.
//!
//! 1. The timer, every 30 s, asks the approval queue to expire what is past its
//!    900 s (an idle serve would otherwise never announce an expiry), which
//!    pushes the `approval_resolved` events through the notifier.
//! 2. A worker takes its destination's oldest due event and sends it with no
//!    lock held, one request in flight per destination: the notifier (which
//!    runs on the approval's own thread) only ever contends with the queue's
//!    mutex for a push, never with a send. After a transient failure the
//!    worker stops for this pass and looks again a second later.
//!
//! Retries are 2 s, 10 s, 30 s and 2 min after a transient failure, then the
//! event is given up; an event is never sent past its deadline (a stale
//! `approval_requested` is worse than none). Nothing a receiver says is read
//! or printed; a failure is one stderr line with the destination NAME and a
//! closed code. The sender never decides, grants or changes an approval: it
//! only tells a gateway that one exists, and the only way to decide stays
//! `POST /approvals/{id}/decision` with the serve credential.

use std::collections::BTreeSet;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use super::send::{classify, EnvFileKeys, Keys, NetPoster, Poster, Request, Verdict};
use super::sign;
use super::turn::{self, TurnNotifier};
use super::volatile::{notifier, turn_notifier, Queued, VolatileQueue};
use super::{Destinations, Event};
use crate::approvals::ApprovalQueue;

/// How often an idle serve expires unanswered approvals.
pub const EXPIRE_EVERY_SECS: i64 = 30;
/// The wait after the nth failed attempt (1-based); after the last one the
/// event is given up.
const BACKOFF_SECS: [i64; 4] = [2, 10, 30, 120];
/// `Retry-After` on 429/503 raises a wait, up to this.
const MAX_RETRY_AFTER_SECS: i64 = 120;
/// Most sends one look makes, so a burst cannot keep it busy forever.
const MAX_SENDS_PER_LOOK: usize = 1000;
/// How often the timer and each worker look.
const POLL: Duration = Duration::from_secs(1);
/// The closed code for a retry that found its destination's queue full.
const QUEUE_FULL: &str = "queue_full";
/// The events this sender delivers; a destination listing one gets a worker.
const VOLATILE_EVENTS: [Event; 3] = [
    Event::ApprovalRequested,
    Event::ApprovalResolved,
    Event::TurnFinished,
];

/// The wait after the `attempt`th failed attempt (1-based) of `event`; `None`
/// gives the event up. Approval events stop after [`BACKOFF_SECS`]; a
/// `turn_finished` (provisional D1/D9) continues every `turn::RETRY_TAIL_SECS`
/// until its deadline, which the caller enforces.
fn retry_wait(event: Event, attempt: u32) -> Option<i64> {
    let i = attempt as usize - 1;
    match event {
        Event::TurnFinished => Some(
            turn::RETRY_BACKOFF_SECS
                .get(i)
                .copied()
                .unwrap_or(turn::RETRY_TAIL_SECS),
        ),
        _ => BACKOFF_SECS.get(i).copied(),
    }
}

/// What a look did, for tests.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Looked {
    pub sent: usize,
    pub retried: usize,
    pub gave_up: usize,
    pub expired_unsent: usize,
}

/// The state of the sender, so a look is a plain function of its inputs.
pub struct ApprovalSender {
    queue: Arc<VolatileQueue>,
    expire: Box<dyn Fn() + Send>,
    next_expire: i64,
}

impl ApprovalSender {
    /// `expire` is what the timer calls (`ApprovalQueue::expire_due`).
    pub fn new(queue: Arc<VolatileQueue>, expire: Box<dyn Fn() + Send>) -> ApprovalSender {
        ApprovalSender {
            queue,
            expire,
            next_expire: 0,
        }
    }

    /// Runs the expiry timer when it is due at `now`.
    pub fn tick(&mut self, now: i64) {
        if now >= self.next_expire {
            self.next_expire = now.saturating_add(EXPIRE_EVERY_SECS);
            (self.expire)();
        }
    }

    /// One look at `now` on the calling thread: the timer, then every
    /// destination's due events. `failed` is called for each event given up on
    /// or refused for good, with the destination NAME and a closed code. `serve`
    /// does not call this: it runs [`Self::tick`] and [`drain`] on their own
    /// threads (see [`spawn`]).
    pub fn look(
        &mut self,
        dests: &Destinations,
        now: i64,
        poster: &dyn Poster,
        keys: &dyn Keys,
        failed: &mut dyn FnMut(&str, &str),
    ) -> Looked {
        self.tick(now);
        drain(&self.queue, &|_| true, dests, &|| now, poster, keys, failed)
    }
}

/// Sends the due events of the destinations `wanted` accepts, one per
/// destination at a time, oldest first, reading `clock` before each send. A
/// destination that fails transiently is left alone for the rest of this pass,
/// so a dead one costs at most one timeout per pass.
pub fn drain(
    queue: &VolatileQueue,
    wanted: &dyn Fn(&str) -> bool,
    dests: &Destinations,
    clock: &dyn Fn() -> i64,
    poster: &dyn Poster,
    keys: &dyn Keys,
    failed: &mut dyn FnMut(&str, &str),
) -> Looked {
    let mut out = Looked::default();
    let mut backing_off = BTreeSet::new();
    let mut sends = 0;
    loop {
        let taken = queue.take_due_where(clock(), &|d| wanted(d) && !backing_off.contains(d));
        out.expired_unsent += taken.expired.len();
        if taken.due.is_empty() {
            return out;
        }
        for ev in taken.due {
            sends += 1;
            let dest = ev.dest.clone();
            if send_one(queue, dests, clock(), poster, keys, failed, ev, &mut out) {
                backing_off.insert(dest);
            }
        }
        if sends >= MAX_SENDS_PER_LOOK {
            return out;
        }
    }
}

/// Sends `ev` once. True when it failed transiently (whether or not it could
/// be retried).
#[allow(clippy::too_many_arguments)]
fn send_one(
    queue: &VolatileQueue,
    dests: &Destinations,
    now: i64,
    poster: &dyn Poster,
    keys: &dyn Keys,
    failed: &mut dyn FnMut(&str, &str),
    mut ev: Queued,
    out: &mut Looked,
) -> bool {
    // The destination and what it lists come from the loaded config, as for
    // a cron event (the body was built here, but the map is an argument).
    let Some(d) = dests.get(&ev.dest).filter(|d| d.events.contains(&ev.event)) else {
        return false; // no longer configured: nothing to send to
    };
    let verdict = match keys.key(d) {
        Err(code) => Verdict::Retry {
            code: code.into(),
            retry_after: None,
        },
        Ok(key) => {
            let headers =
                sign::request_headers(&key, now, &ev.id, ev.event.as_str(), ev.body.as_bytes());
            let req = Request {
                url: &d.url,
                body: ev.body.as_bytes(),
                headers: &headers,
                timeout: d.timeout,
                policy: d.policy,
            };
            classify(&poster.post(&req))
        }
    };
    match verdict {
        Verdict::Done => out.sent += 1,
        Verdict::Dead(code) => {
            out.gave_up += 1;
            failed(&ev.dest, &code);
        }
        Verdict::Retry { code, retry_after } => {
            ev.attempts += 1;
            let Some(wait) = retry_wait(ev.event, ev.attempts) else {
                out.gave_up += 1;
                failed(&ev.dest, &code);
                return true;
            };
            let wait = wait.max(retry_after.unwrap_or(0).min(MAX_RETRY_AFTER_SECS));
            ev.next_at = now.saturating_add(wait);
            if ev.next_at >= ev.deadline {
                // It would be stale by then: not worth sending. A lost turn
                // end leaves a user waiting, so it is reported (a name and
                // the last closed code); an approval event is not.
                out.expired_unsent += 1;
                if ev.event == Event::TurnFinished {
                    failed(&ev.dest, &code);
                }
                return true;
            }
            let dest = ev.dest.clone();
            if queue.requeue(ev).is_ok() {
                out.retried += 1;
            } else {
                // The retry is the one dropped (and counted by the queue),
                // never a newer event; it is given up and reported.
                out.gave_up += 1;
                failed(&dest, QUEUE_FULL);
            }
            return true;
        }
    }
    false
}

/// What [`spawn`] installed.
pub struct Spawned {
    /// For the serve turn path, when a destination lists `turn_finished`.
    pub turns: Option<TurnNotifier>,
}

/// Installs the notifiers and starts the sender threads (#911, #962, #980):
/// the timer and one worker per destination that lists an approval or turn
/// event. Does nothing when no destination lists one, so a serve without one
/// is exactly as before. The approval notifier and the expiry call are only
/// installed when an approval event is listed. The threads are never joined:
/// pending events are volatile and die with the process.
pub fn spawn(dests: Arc<Destinations>, approvals: Arc<ApprovalQueue>) -> Spawned {
    let approval_events = dests.any_lists(&[Event::ApprovalRequested, Event::ApprovalResolved]);
    let turn_events = dests.any_lists(&[Event::TurnFinished]);
    if !approval_events && !turn_events {
        return Spawned { turns: None };
    }
    let queue = VolatileQueue::new();
    if approval_events {
        approvals.set_notifier(Some(notifier(
            Arc::clone(&dests),
            Arc::clone(&queue),
            Arc::new(unix_now),
            Arc::new(tole_core::secrets::process_secrets),
        )));
    }
    let turns = turn_events.then(|| {
        turn_notifier(
            Arc::clone(&dests),
            Arc::clone(&queue),
            Arc::new(unix_now),
            Arc::new(tole_core::secrets::process_secrets),
        )
    });
    let expire = Arc::clone(&approvals);
    let sender = ApprovalSender::new(
        Arc::clone(&queue),
        Box::new(move || {
            // Without an approval event no notifier is installed: leave the
            // queue's own lazy pruning as it was.
            if approval_events {
                expire.expire_due();
            }
        }),
    );
    start(
        dests,
        sender,
        Wiring {
            poster: Arc::new(NetPoster),
            keys: Arc::new(EnvFileKeys),
            clock: Arc::new(unix_now),
            failed: Arc::new(|dest, code| {
                // A name and a closed code: never a URL, a header or text a
                // receiver sent.
                eprintln!("tole serve: webhook event to `{dest}` failed: {code}");
            }),
            stop: Arc::new(AtomicBool::new(false)),
        },
    );
    Spawned { turns }
}

/// Reports an event given up on: the destination NAME and a closed code.
pub(crate) type Failed = dyn Fn(&str, &str) + Send + Sync;

/// What the sender threads use: the real network, keys and clock in [`spawn`],
/// fakes in tests. `stop` ends the threads after their current pass (serve
/// never sets it; pending events are volatile and die with the process).
pub(crate) struct Wiring {
    pub poster: Arc<dyn Poster + Send + Sync>,
    pub keys: Arc<dyn Keys + Send + Sync>,
    pub clock: Arc<dyn Fn() -> i64 + Send + Sync>,
    pub failed: Arc<Failed>,
    pub stop: Arc<AtomicBool>,
}

/// Starts the timer thread and one worker per destination that lists a
/// volatile event, and returns how many workers started. The threads are never
/// joined.
pub(crate) fn start(dests: Arc<Destinations>, mut sender: ApprovalSender, w: Wiring) -> usize {
    let queue = Arc::clone(&sender.queue);
    let mut workers = 0;
    for name in dests.names() {
        let Some(d) = dests.get(name) else { continue };
        if !VOLATILE_EVENTS.iter().any(|e| d.events.contains(e)) {
            continue;
        }
        let name = name.to_string();
        let (dests, queue) = (Arc::clone(&dests), Arc::clone(&queue));
        let (poster, keys, clock, failed, stop) = (
            Arc::clone(&w.poster),
            Arc::clone(&w.keys),
            Arc::clone(&w.clock),
            Arc::clone(&w.failed),
            Arc::clone(&w.stop),
        );
        let worker = std::thread::Builder::new()
            .name(format!("tole-webhook-{name}"))
            .spawn(move || {
                while !stop.load(Ordering::Relaxed) {
                    let pass = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        drain(
                            &queue,
                            &|d| d == name,
                            &dests,
                            &*clock,
                            &*poster,
                            &*keys,
                            &mut |d, c| failed(d, c),
                        )
                    }));
                    if pass.is_err() {
                        eprintln!("tole serve: the webhook event sender for `{name}` hit an internal error");
                    }
                    std::thread::sleep(POLL);
                }
            });
        match worker {
            Ok(_) => workers += 1,
            Err(_) => {
                eprintln!("tole serve: could not start the webhook event sender for a destination")
            }
        }
    }
    let (clock, stop) = (w.clock, w.stop);
    let timer = std::thread::Builder::new()
        .name("tole-webhook-timer".into())
        .spawn(move || {
            let mut dropped_seen = 0;
            while !stop.load(Ordering::Relaxed) {
                let tick =
                    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| sender.tick(clock())));
                if tick.is_err() {
                    eprintln!("tole serve: the webhook approval timer hit an internal error");
                }
                let dropped = queue.dropped();
                if dropped != dropped_seen {
                    eprintln!(
                        "tole serve: {} webhook event(s) dropped, a destination's queue is full",
                        dropped - dropped_seen
                    );
                    dropped_seen = dropped;
                }
                std::thread::sleep(POLL);
            }
        });
    if timer.is_err() {
        eprintln!("tole serve: could not start the webhook approval timer");
    }
    workers
}

fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64)
}

#[cfg(test)]
#[path = "approval_sender_tests.rs"]
mod tests;
