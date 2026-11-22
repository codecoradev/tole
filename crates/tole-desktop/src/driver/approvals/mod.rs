//! The approvals driver: keeps the model's `ApprovalsState` true to a serve
//! and carries out the owner's answers.
//!
//! It connects the adapter's pure functions (`adapter::approvals`) to the
//! blocking client, and follows the contract written there: after every loss of
//! knowledge it applies `ConnectionLost` and then fetches the whole list with a
//! fresh fetch id, it retries a failed fetch with `Backoff` for ever, and it
//! ticks the model. The rules are in `engine` (`Engine::step`), which has no
//! thread and no I/O and is what the tests drive.
//!
//! # Thread model
//!
//! Five kinds of thread, all started by [`ApprovalsDriver::start`], all
//! stopped by [`ApprovalsDriver::shutdown`]:
//!
//! | thread | does | blocks on |
//! |---|---|---|
//! | `worker` (1) | reads one bounded inbox, runs `Engine::step`, hands the effects to the I/O threads, tells the UI | the inbox |
//! | `reader` (1 per followed session, at most [`MAX_STREAMS`]) | follows a session's event stream, reopens it with a backoff, retires after repeated "gone" answers | the stream |
//! | `fetcher` (1) | `GET /approvals`, one at a time | the call |
//! | `decider` (1) | `POST .../decision`, one at a time | the call |
//! | `ticker` (1) | wakes the worker once a second | the clock |
//!
//! Only the worker changes the state, and it does so one input at a time. The
//! state sits behind a mutex that is held for the apply and nothing else (never
//! across a call or a send), so [`Handle::snapshot`] from the UI thread waits at
//! most that long. A call that blocks blocks its own thread, not the worker:
//! the stream and the owner's presses go on, and the worker ticks the model
//! from its own clock reading between inputs, so time reaches the model at
//! least once a second however much is queued. The inbox is bounded (`INBOX`):
//! a producer waits when it is full, except the ticker and the UI, which drop
//! the message ([`Busy`] for the UI). A call is bounded by the client (see
//! [`ClientApi`](super::api::ClientApi): 30 s per read and per body, so about a
//! minute at worst); a hung call holds its slot (one fetch, one decision at a
//! time) until it returns.
//!
//! # Panics
//!
//! A panic inside an `Api` call is caught where the call is made and is that
//! call's failure (the engine releases the slot, retries, shows the card as
//! waiting again, or treats the stream as lost); the thread goes on. A panic
//! anywhere else in a thread is reported to the worker, which ends the driver:
//! it stops the other threads and clears the state, so the view reads `Unknown`
//! and [`Handle::is_running`] is false.
//!
//! # Stopping
//!
//! `shutdown` raises one stop signal (it wakes every sleep) and joins the
//! threads for up to a timeout. A thread inside a call is not interrupted (the
//! client has no cancel): it is reported as `detached`, ends when the call
//! returns, and sends nothing after the signal. When the worker ends it clears
//! the state, so a view read afterwards is `Unknown`, never a stale count.
//!
//! # When the list is known
//!
//! The serve announces approvals on a session's stream, one stream per
//! session, so the driver follows the sessions in [`Config::sessions`] (at most
//! [`MAX_STREAMS`]; the set is fixed for the life of the driver, restart it to
//! change it). The badge is a number and a box is open only when:
//!
//! - a fetch of the whole list that STARTED after every followed stream was
//!   up has succeeded (a stream that dropped keeps the view `Unknown` until it
//!   is up again and a fetch has followed, however fast the fetch is), and
//! - that success is not older than twice [`Config::refresh`].
//!
//! A request in a session the driver does not follow (a task opened later, or
//! past the cap) has no stream: it is found by the periodic refresh, so it can
//! be missing from the view for up to one refresh interval (15 s by default;
//! never turned off). A stream the client reconnects by itself is invisible
//! until it delivers an item, so a drop there is bounded by the same refresh
//! and age rules. A page that shows the badge as a number is therefore
//! showing "as of at most about one refresh interval ago", and should say
//! so.
//!
//! A followed session whose stream the serve refuses three times in a row (404
//! after a serve restart, until the task is resumed; `closed`; 429 when the
//! serve's eight stream slots are taken) is parked: it no longer holds the list
//! unknown, it is listed in [`ApprovalsView::parked_sessions`] (covered by the
//! refresh only, so up to one refresh interval late), and its reader probes it
//! every 30 s or so, rejoining the coverage as soon as a stream opens. A reader
//! never gives up its session.
//!
//! The wall clock moving ahead of the monotonic one by more than a few seconds
//! (the machine slept, the clock was corrected) counts as a loss: the list is
//! not known until a fetch says so. A stream a remote serve dropped during the
//! sleep is noticed by its reader's error, within the stream's idle timeout.
//!
//! # Limits
//!
//! - A reader blocked in a read is detached at shutdown and keeps its stream
//!   slot on the serve until its next frame (a heartbeat, at most 15 s) or the
//!   45 s idle timeout, so a driver restarted at once may find its streams
//!   refused (429) and park them until the old ones let go.
//! - One `GET /approvals` is out at a time, so after a loss a hung one (the
//!   client bounds it at about a minute) delays the next; the view stays
//!   `Unknown` meanwhile.

mod engine;
mod store;
mod threads;
mod view;
mod worker;

#[cfg(test)]
mod coverage_tests;
#[cfg(test)]
mod decision_tests;
#[cfg(test)]
mod e2e_end_tests;
#[cfg(test)]
mod e2e_tests;
#[cfg(test)]
mod engine_tests;
#[cfg(test)]
mod parked_tests;
#[cfg(test)]
mod rig;

use std::io;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{sync_channel, SyncSender};
use std::sync::Arc;
use std::time::Duration;

use tole_desktop_model::approvals::{ComposerState, Decision, Tab};

use self::engine::{Engine, Input, TICK_EVERY};
use self::store::Store;
use self::threads::{decider, fetcher, reader};
pub use self::view::ApprovalsView;
use self::worker::Worker;
use super::api::Api;
use super::clock::{Clock, Shutdown};
use super::runtime::{JoinReport, Threads};
use super::shared::handle::{self, clamp_refresh};
pub use super::shared::handle::{Busy, OnChange};
use super::shared::thread::{run_guarded, spawn_ticker, stop_threads, DROP_JOIN};

/// Most event streams one driver holds. The serve allows 8 for all its
/// clients together, so a desktop with many tasks open must not take them all.
pub const MAX_STREAMS: usize = 4;
/// The refresh a driver makes when [`Config::refresh`] is left alone.
pub const DEFAULT_REFRESH: Duration = Duration::from_secs(15);
/// The shortest refresh a driver accepts (a shorter one is raised to this).
pub const MIN_REFRESH: Duration = Duration::from_secs(1);
/// The longest refresh a driver accepts (a longer one is lowered to this): the
/// refresh and the age rule that follows it (a list older than twice the
/// refresh is not known) would otherwise mean nothing.
pub const MAX_REFRESH: Duration = Duration::from_secs(60);
/// The room in the worker's inbox.
const INBOX: usize = 256;

/// The refresh a driver uses: `refresh` held between [`MIN_REFRESH`] and
/// [`MAX_REFRESH`].
pub(super) fn effective_refresh(refresh: Duration, ceiling: Duration) -> Duration {
    clamp_refresh(refresh, MIN_REFRESH, ceiling)
}

/// The sessions to follow: no empty id, none twice (two readers of one session
/// would share one coverage flag and one stream slot of the serve's eight), in
/// the given order, at most [`MAX_STREAMS`].
pub(super) fn followed_sessions(sessions: Vec<String>) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    (sessions.into_iter())
        .filter(|s| !s.is_empty() && seen.insert(s.clone()))
        .take(MAX_STREAMS)
        .collect()
}

pub struct Config {
    /// The sessions whose streams carry approval frames. Streams past
    /// [`MAX_STREAMS`] are ignored.
    pub sessions: Vec<String>,
    /// `GET /approvals` this often when nothing failed (held between
    /// [`MIN_REFRESH`] and [`MAX_REFRESH`]). Not optional: it is the only way a request of a
    /// session with no stream is found, and a list older than twice this is no
    /// longer shown as known.
    pub refresh: Duration,
    pub on_change: Option<OnChange>,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            sessions: Vec::new(),
            refresh: DEFAULT_REFRESH,
            on_change: None,
        }
    }
}

/// The UI's side of the driver: cheap to clone, usable from any thread.
#[derive(Clone)]
pub struct Handle {
    store: Arc<Store>,
    inbox: SyncSender<Input>,
    clock: Arc<dyn Clock>,
    alive: Arc<AtomicBool>,
}

impl Handle {
    /// The model as of now (the countdowns use the clock's time).
    pub fn snapshot(&self) -> ApprovalsView {
        self.store.view(self.clock.now_ms())
    }

    /// Whether the message box of this task works. `Checking` until the list
    /// is known, and after the driver stopped.
    pub fn composer(&self, task: &str) -> ComposerState {
        self.store.lock().composer(task)
    }

    /// The owner pressed a button on request `id`. Sent at most once: a request
    /// that is not waiting, is already being answered or ran out of time is
    /// refused by the model and nothing is sent. The outcome shows in the view.
    pub fn decide(&self, id: &str, decision: Decision) -> Result<(), Busy> {
        self.send(Input::Decide(id.to_owned(), decision))
    }

    pub fn select(&self, id: Option<String>) -> Result<(), Busy> {
        self.send(Input::Select(id))
    }

    pub fn select_tab(&self, tab: Tab) -> Result<(), Busy> {
        self.send(Input::SelectTab(tab))
    }

    /// Whether the worker is running.
    pub fn is_running(&self) -> bool {
        self.alive.load(Ordering::SeqCst)
    }

    fn send(&self, input: Input) -> Result<(), Busy> {
        handle::queue(&self.inbox, input)
    }
}

/// The running driver: owns its threads.
pub struct ApprovalsDriver {
    handle: Handle,
    stop: Shutdown,
    threads: Threads,
}

impl ApprovalsDriver {
    /// Start the threads. The first thing the worker does is fetch the whole
    /// list; until that succeeds the view is `Unknown` and every box `Checking`.
    /// `Err` only when the system cannot start a thread (nothing is left
    /// running then).
    pub fn start(api: Arc<dyn Api>, clock: Arc<dyn Clock>, cfg: Config) -> io::Result<Self> {
        Self::start_with_ceiling(api, clock, cfg, MAX_REFRESH)
    }

    /// [`start`](Self::start) with another ceiling for the refresh: for a test
    /// whose clock is moved fast and must not reach the refresh by accident.
    pub(super) fn start_with_ceiling(
        api: Arc<dyn Api>,
        clock: Arc<dyn Clock>,
        cfg: Config,
        ceiling: Duration,
    ) -> io::Result<Self> {
        let store = Arc::new(Store::default());
        let stop = Shutdown::new();
        let alive = Arc::new(AtomicBool::new(true));
        let (inbox, inbox_rx) = sync_channel::<Input>(INBOX);
        let (fetch_tx, fetch_rx) = sync_channel(1);
        let (send_tx, send_rx) = sync_channel(1);
        let handle = Handle {
            store: Arc::clone(&store),
            inbox: inbox.clone(),
            clock: Arc::clone(&clock),
            alive: Arc::clone(&alive),
        };
        let mut driver = ApprovalsDriver {
            handle,
            stop: stop.clone(),
            threads: Threads::new(),
        };
        let sessions = followed_sessions(cfg.sessions);
        let started = (|| {
            let t = &mut driver.threads;
            let worker = Worker {
                engine: Engine::new(
                    Arc::clone(&store),
                    sessions.clone(),
                    effective_refresh(cfg.refresh, ceiling),
                ),
                inbox: inbox_rx,
                fetch: fetch_tx,
                send: send_tx,
                clock: Arc::clone(&clock),
                stop: stop.clone(),
                store: Arc::clone(&store),
                alive,
                on_change: cfg.on_change,
            };
            t.spawn("approvals-worker", move || worker.run())?;
            let (a, i, s) = (Arc::clone(&api), inbox.clone(), stop.clone());
            t.spawn("approvals-fetch", move || {
                run_guarded(&i.clone(), || fetcher(a, fetch_rx, i, s))
            })?;
            let (a, i, s) = (Arc::clone(&api), inbox.clone(), stop.clone());
            t.spawn("approvals-decide", move || {
                run_guarded(&i.clone(), || decider(a, send_rx, i, s))
            })?;
            let (i, c, s) = (inbox.clone(), Arc::clone(&clock), stop.clone());
            spawn_ticker(t, "approvals-tick", i, c, s, TICK_EVERY)?;
            for session in sessions {
                let (a, i, c, s) = (
                    Arc::clone(&api),
                    inbox.clone(),
                    Arc::clone(&clock),
                    stop.clone(),
                );
                t.spawn("approvals-stream", move || {
                    run_guarded(&i.clone(), || reader(a, session, i, c, s))
                })?;
            }
            io::Result::Ok(())
        })();
        match started {
            Ok(()) => Ok(driver),
            Err(e) => {
                driver.stop(DROP_JOIN);
                Err(e)
            }
        }
    }

    /// The UI's side. Clones stay usable after the driver stops: they read
    /// `Unknown` and refuse commands.
    pub fn handle(&self) -> Handle {
        self.handle.clone()
    }

    /// Stop every thread and wait up to `timeout` for them.
    pub fn shutdown(mut self, timeout: Duration) -> JoinReport {
        self.stop(timeout)
    }

    fn stop(&mut self, timeout: Duration) -> JoinReport {
        stop_threads(&self.stop, &self.handle.inbox, &mut self.threads, timeout)
    }
}

impl Drop for ApprovalsDriver {
    fn drop(&mut self) {
        self.stop(DROP_JOIN);
    }
}
