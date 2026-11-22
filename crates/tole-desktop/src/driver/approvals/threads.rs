//! The bodies of the driver's threads. None decides anything: they block on a
//! call or a sleep and report what came back to the worker as an [`Input`].
//!
//! Each ends when the stop signal is raised, or when the worker is gone (its
//! inbox, or the channel it feeds this thread through, is closed).
//!
//! A panic never goes unnoticed. A panic inside an `Api` call is caught where
//! the call is made and becomes the failure of that call (`Fetched` or
//! `Decided` with an error, a lost stream), so the thread goes on and the
//! engine releases its slot and retries. A panic anywhere else in a thread is
//! caught by [`run_guarded`], which tells the worker (`ThreadFailed`); the
//! worker then ends the driver, which leaves the view `Unknown`.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::io;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::mpsc::{Receiver, SyncSender, TrySendError};
use std::sync::Arc;
use std::time::Duration;

use tole_client::{Error, Event, StreamItem};
use tole_desktop_model::approvals::{DecisionIntent, FetchId};

use super::engine::{Input, TICK_EVERY};
use crate::adapter::approvals::{
    decision_to_client, row_for_decision, stream_actions, Backoff, RETRY_MAX,
};
use crate::driver::api::{Api, NO_REPLAY};
use crate::driver::clock::{Clock, Shutdown, Slept};

/// Consecutive refusals (a 404, a 429, a `closed` frame) after which a reader
/// parks its session.
pub(super) const PARK_AFTER: u32 = 3;
/// How often a parked session's stream is probed (plus a spread of up to 10 s).
pub(super) const PROBE_EVERY: Duration = Duration::from_secs(30);

/// Run a thread body; if it panics, tell the worker so the driver ends.
pub(super) fn run_guarded(inbox: &SyncSender<Input>, body: impl FnOnce()) {
    if catch_unwind(AssertUnwindSafe(body)).is_err() {
        let _ = inbox.send(Input::ThreadFailed);
    }
}

/// A call into the `Api`, with a panic turned into `None`.
fn guarded<T>(call: impl FnOnce() -> T) -> Option<T> {
    catch_unwind(AssertUnwindSafe(call)).ok()
}

fn panicked() -> Error {
    Error::Io(io::Error::other("the call panicked"))
}

pub(super) fn ticker(inbox: SyncSender<Input>, clock: Arc<dyn Clock>, stop: Shutdown) {
    while clock.sleep(TICK_EVERY, &stop) == Slept::Elapsed {
        // A full inbox drops the tick: the worker keeps its own time and
        // ticks the model between inputs, so this only wakes it when idle.
        if let Err(TrySendError::Disconnected(_)) = inbox.try_send(Input::Tick) {
            return;
        }
    }
}

/// Runs the `GET /approvals` calls the engine asks for, one at a time.
pub(super) fn fetcher(
    api: Arc<dyn Api>,
    jobs: Receiver<FetchId>,
    inbox: SyncSender<Input>,
    stop: Shutdown,
) {
    for id in jobs {
        if stop.is_set() {
            return;
        }
        let reply = guarded(|| api.approvals(None)).unwrap_or_else(|| Err(panicked()));
        if inbox.send(Input::Fetched(id, reply)).is_err() {
            return;
        }
    }
}

/// Sends the decisions the engine asks for, one at a time.
pub(super) fn decider(
    api: Arc<dyn Api>,
    jobs: Receiver<DecisionIntent>,
    inbox: SyncSender<Input>,
    stop: Shutdown,
) {
    for intent in jobs {
        if stop.is_set() {
            return;
        }
        let row = row_for_decision(&intent);
        let verdict = decision_to_client(intent.decision);
        let reply = guarded(|| api.decide_row(&row, verdict)).unwrap_or_else(|| Err(panicked()));
        if inbox.send(Input::Decided(intent, reply)).is_err() {
            return;
        }
    }
}

/// Whether the serve refused the stream (404 unknown session, 429 no free
/// stream slot, any other status), as opposed to a network failure.
fn refused(e: &Error) -> bool {
    matches!(e, Error::Status { .. })
}

/// The wait between two probes of a parked session: [`PROBE_EVERY`] plus a
/// spread of up to 10 s that depends on the session and the attempt, so
/// readers that parked together do not probe together.
fn probe_wait(session: &str, attempt: u32) -> Duration {
    let mut h = DefaultHasher::new();
    (session, attempt).hash(&mut h);
    PROBE_EVERY + Duration::from_millis(h.finish() % 10_000)
}

/// Follows one session's stream for the approval frames and opens it again,
/// with a growing wait, whenever it ends or fails. The worker is told when the
/// stream is up (its first item arrived) and when it is down, and the wait
/// keeps a server that closes at once from being hammered; it starts over only
/// after a connection that lived as long as the longest retry wait.
///
/// A reader never gives up its session. After [`PARK_AFTER`] attempts in a row
/// that the serve refused (404 after a serve restart until the owner resumes
/// the task, a `closed` frame, 429 when the serve's stream slots are taken, any
/// other status) it parks: the worker stops waiting for that stream (the
/// session is covered by the refresh only, and the view lists it), and the
/// reader probes it every [`PROBE_EVERY`] or so, silently, until a stream opens
/// again, when it tells the worker and the session rejoins the coverage. Any
/// attempt that is not a refusal starts the count over.
pub(super) fn reader(
    api: Arc<dyn Api>,
    session: String,
    inbox: SyncSender<Input>,
    clock: Arc<dyn Clock>,
    stop: Shutdown,
) {
    let healthy_ms = u64::try_from(RETRY_MAX.as_millis()).unwrap_or(u64::MAX);
    let mut backoff = Backoff::new();
    let mut refusals = 0u32;
    let mut parked = false;
    let mut probes = 0u32;
    while !stop.is_set() {
        let opened = clock.monotonic_ms();
        let source = guarded(|| api.follow_events(&session, NO_REPLAY));
        let (mut up, mut refused_open, mut closed_seen) = (false, false, false);
        if let Some(mut source) = source {
            loop {
                match guarded(|| source.next_item()) {
                    Some(Ok(Some(item))) => {
                        if stop.is_set() {
                            return;
                        }
                        if !up {
                            up = true;
                            parked = false;
                            if inbox.send(Input::StreamUp(session.clone())).is_err() {
                                return;
                            }
                        }
                        closed_seen |= matches!(item, StreamItem::Event(Event::Closed { .. }));
                        // Only what the queue can use goes through the worker's
                        // inbox: not entries, text or heartbeats.
                        if !stream_actions(&item, 0).is_empty()
                            && inbox.send(Input::Stream(item)).is_err()
                        {
                            return;
                        }
                    }
                    Some(Err(e)) => {
                        refused_open = refused(&e);
                        break;
                    }
                    // A clean end, or a panic in the source.
                    Some(Ok(None)) | None => break,
                }
            }
        }
        refusals = if refused_open || closed_seen {
            refusals.saturating_add(1)
        } else {
            0
        };
        // A probe of a parked session that did not open says nothing new.
        let silent = parked && !up;
        if stop.is_set() || (!silent && inbox.send(Input::StreamDown(session.clone())).is_err()) {
            return;
        }
        if clock.monotonic_ms().saturating_sub(opened) >= healthy_ms {
            backoff.succeeded();
        }
        if !parked && refusals >= PARK_AFTER {
            parked = true;
            if inbox.send(Input::StreamParked(session.clone())).is_err() {
                return;
            }
        }
        let wait = if parked {
            probes = probes.saturating_add(1);
            probe_wait(&session, probes)
        } else {
            probes = 0;
            backoff.failed()
        };
        if clock.sleep(wait, &stop) == Slept::Stopped {
            return;
        }
    }
}
