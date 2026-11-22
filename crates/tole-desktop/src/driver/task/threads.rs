//! The bodies of the driver's threads. None decides anything: they block on a
//! call, a stream or a sleep and report what came back to the worker as an
//! [`Input`].
//!
//! Each ends when the stop signal is raised, or when the worker is gone (its
//! inbox, or the channel it feeds this thread through, is closed). A panic
//! inside an `Api` call is that call's failure (`calls::run`, and the stream's
//! own guard below); a panic anywhere else is caught by `run_guarded` (`driver::shared::thread`), which
//! tells the worker (`ThreadFailed`), and the worker ends the driver.

use std::io;
use std::sync::mpsc::{Receiver, SyncSender};
use std::sync::Arc;

use tole_client::{Error, Event, StreamItem};

use super::calls::{self, Call, Lane};
use super::engine::Input;
use crate::adapter::approvals::{Backoff, RETRY_MAX};
use crate::driver::api::Api;
use crate::driver::clock::{millis, Clock, Shutdown, Slept};
use crate::driver::guard::{guarded, panicked, probe_wait, refused, PARK_AFTER};
use crate::driver::runtime::Spawner;
use crate::driver::shared::thread::run_guarded;

/// Start the thread of `kind`, to take its calls from `jobs`.
pub(super) fn spawn_lane(
    spawner: &Spawner,
    api: Arc<dyn Api>,
    session: String,
    kind: Lane,
    jobs: Receiver<Call>,
    (inbox, stop): (SyncSender<Input>, Shutdown),
) -> io::Result<()> {
    spawner.spawn(kind.name(), move || {
        run_guarded(&inbox.clone(), || lane(api, session, jobs, inbox, stop))
    })
}

/// Makes the calls of one lane, one at a time.
fn lane(
    api: Arc<dyn Api>,
    session: String,
    jobs: Receiver<Call>,
    inbox: SyncSender<Input>,
    stop: Shutdown,
) {
    for call in jobs {
        if stop.is_set() {
            return;
        }
        let reply = calls::run(&*api, &session, call);
        if inbox.send(Input::Reply(reply)).is_err() {
            return;
        }
    }
}

fn ended() -> Error {
    Error::Io(io::Error::other("the stream ended"))
}

/// Follows the session's event stream from the cursor the history ended on,
/// and opens it again, with a growing wait, whenever it ends or fails. The
/// worker is told when a stream is up (its first item arrived), every item
/// and every loss.
///
/// A reader never gives up the session, short of the serve's `closed` frame
/// (the session left; there is nothing more to follow). After [`PARK_AFTER`]
/// attempts in a row that the serve refused (404 after a restart until the
/// owner resumes the task, 429 when its stream slots are taken, any other
/// status) it parks: the worker is told once, and the reader probes every
/// `PROBE_EVERY` or so, silently, until a stream opens. It reopens after the
/// last entry it delivered, so no entry is missed or repeated.
pub(super) fn reader(
    api: Arc<dyn Api>,
    session: String,
    go: Receiver<u64>,
    inbox: SyncSender<Input>,
    clock: Arc<dyn Clock>,
    stop: Shutdown,
) {
    // The history decides where the stream starts; no `go` means it never will.
    let Ok(mut last) = go.recv() else { return };
    let healthy_ms = millis(RETRY_MAX);
    let mut backoff = Backoff::new();
    let (mut refusals, mut probes) = (0u32, 0u32);
    let (mut parked, mut had_loss) = (false, false);
    while !stop.is_set() {
        let opened = clock.monotonic_ms();
        let source = guarded(|| api.follow_events(&session, last));
        let (mut up, mut refused_open) = (false, false);
        let over: Error = match source {
            None => panicked(),
            Some(mut source) => loop {
                match guarded(|| source.next_item()) {
                    Some(Ok(Some(item))) => {
                        if stop.is_set() {
                            return;
                        }
                        if !up {
                            up = true;
                            parked = false;
                            let reopened = std::mem::replace(&mut had_loss, false);
                            if inbox.send(Input::Up { reopened }).is_err() {
                                return;
                            }
                        }
                        if let StreamItem::Event(Event::Entry(e)) = &item {
                            last = last.max(e.seq);
                        }
                        let closed = matches!(item, StreamItem::Event(Event::Closed { .. }));
                        if matches!(item, StreamItem::Heartbeat) {
                            continue;
                        }
                        if inbox.send(Input::Stream(Ok(Some(item)))).is_err() || closed {
                            return;
                        }
                    }
                    Some(Err(e)) => {
                        refused_open = refused(&e);
                        break e;
                    }
                    // A clean end without `closed`, or a panic in the source.
                    Some(Ok(None)) => break ended(),
                    None => break panicked(),
                }
            },
        };
        refusals = if refused_open {
            refusals.saturating_add(1)
        } else {
            0
        };
        // A probe of a parked session that did not open says nothing new.
        let silent = parked && !up;
        had_loss = true;
        if !silent && (stop.is_set() || inbox.send(Input::Stream(Err(over))).is_err()) {
            return;
        }
        if clock.monotonic_ms().saturating_sub(opened) >= healthy_ms {
            backoff.succeeded();
        }
        if !parked && refusals >= PARK_AFTER {
            parked = true;
            if inbox.send(Input::Parked).is_err() {
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
