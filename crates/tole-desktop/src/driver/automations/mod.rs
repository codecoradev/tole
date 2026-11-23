//! The automations driver: keeps the model's `AutomationsState` (the
//! Automations page, board S6, #1033) true to a serve, and carries the owner's
//! switch.
//!
//! It connects the adapter's pure functions (`adapter::automations`) to the
//! blocking client. It loads `GET /cron/jobs` when it starts, again on a slow
//! timer ([`Config::refresh`], 30 s by default) and whenever the owner asks
//! ([`Handle::refresh`]); it loads `GET /cron/runs` of the automation the owner
//! opened ([`Handle::select`]) the same way; it pauses or resumes a job once per
//! press ([`Handle::set_enabled`]); it retries a failed read with a backoff for
//! ever, and hands the UI a plain-data [`AutomationsView`]. The rules are in
//! `engine` (`Engine::step`), which has no thread and no I/O and is what the
//! tests drive.
//!
//! The scheduler is not here, and never in the UI: the jobs run in the serve
//! (`tole serve --cron`) or in a `tole cron tick`; this driver reads what they
//! did and flips the same switch `tole cron pause` and `resume` write (the
//! serve routes write the job state `tole cron` reads, so a switch used here
//! is the state a terminal sees, and the other way round at the next read).
//!
//! # Thread model
//!
//! Five threads, all started by [`AutomationsDriver::start`], all stopped by
//! [`AutomationsDriver::shutdown`]:
//!
//! | thread | does | blocks on |
//! |---|---|---|
//! | `worker` (1) | reads one bounded inbox, runs `Engine::step`, hands the effects to the I/O threads, tells the UI | the inbox |
//! | `jobs loader` (1) | `GET /cron/jobs`, one call at a time | the call |
//! | `runs loader` (1) | `GET /cron/runs`, and `GET /approvals` when a run stopped to ask, one at a time | the call |
//! | `toggler` (1) | `POST .../pause` or `.../resume`, one at a time, each once | the call |
//! | `ticker` (1) | wakes the worker once a second | the clock |
//!
//! Only the worker changes the state, and it does so one input at a time. The
//! state sits behind a mutex that is held for the apply and nothing else (never
//! across a call or a send), and the UI's callback runs after it is released,
//! so [`Handle::snapshot`] from the UI thread waits at most that long. A call
//! that blocks blocks its own thread, not the worker nor the other routes. The
//! inbox is bounded (`INBOX`): an I/O thread waits when it is full (an answer is
//! never dropped), the ticker drops its message, and the UI gets [`Busy`]. A
//! call is bounded by the client (see [`ClientApi`](super::api::ClientApi): 30 s
//! per read and per body, so about a minute at worst).
//!
//! # Panics
//!
//! A panic inside an `Api` call is caught where the call is made and is that
//! call's failure (a read is marked failed and retried, a switch shows "could
//! not reach Tole" and the list is read again); the thread goes on. A panic
//! anywhere else in a thread is reported to the worker, which ends the driver:
//! it stops the other threads and clears the state, so the view reads not loaded
//! (no rows) and [`Handle::is_running`] is false.
//!
//! # Stopping
//!
//! `shutdown` raises one stop signal (it wakes every sleep) and joins the
//! threads for up to a timeout. A thread inside a call is not interrupted (the
//! client has no cancel): it is reported as `detached`, ends when the call
//! returns, and sends nothing the worker will read. When the worker ends it
//! clears the state, so a view read afterwards is never a stale list.
//!
//! # When the list is known
//!
//! The rows are the serve's list only after a call succeeded. They stay, marked
//! failed (`page.load` is `Failed`, and the scheduler line says it is not
//! running), when:
//!
//! - the next call fails, whatever the reason (404, no connection, a panic);
//! - the rows are older than twice the refresh interval (calls that hang);
//! - the machine slept, or the wall clock was moved ahead of the monotonic one
//!   by more than a few seconds: the rows may be hours old, and the call that
//!   was out is ignored.
//!
//! [`AutomationsView::counts`] is a number only while the rows are known and
//! are all of the jobs ([`AutomationsView::left_out`] is zero). The history of
//! the selected automation is marked the same way ([`History::load`]).
//!
//! # The switch
//!
//! [`Handle::set_enabled`] makes one request, or none (see `engine`), and the
//! switch of the row changes only when the serve answers, to the state the serve
//! names. A refused switch is a notice in the view (`page.notice`), not a retry:
//! a job Tole made that the owner has not reviewed says so and names the command
//! to run in a terminal, a plan-mode serve says it pauses and does not resume.
//! While the call is out the view says which switch is busy
//! ([`AutomationsView::toggling`]). Whatever the answer, the list is read again
//! at once, and a list call that was out is ignored, so a stale list never puts
//! the old state back under the new one.
//!
//! # Limits
//!
//! - A job created or changed at the terminal shows up at the next refresh (30 s
//!   by default, or at once with [`Handle::refresh`]); there is no event for it.
//! - A hung call holds its route's slot until it returns (the client bounds it
//!   at about a minute), so a refresh after a loss, or the list read that follows
//!   a switch, waits for it.
//! - A plan-mode serve's refusal of a resume (409) blocks every resume until the
//!   driver is restarted (`page.resume_blocked` is the model's and is never
//!   cleared by it); a pause still works.

mod engine;
mod store;
mod threads;
mod view;
mod worker;

use std::io;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{sync_channel, SyncSender};
use std::sync::Arc;
use std::time::Duration;

use self::engine::{Engine, Input, TICK_EVERY};
use self::store::Store;
pub use self::store::{Feed, History};
use self::threads::{jobs_loader, runs_loader, toggler};
pub use self::view::AutomationsView;
use self::worker::Worker;
use super::api::Api;
use super::clock::{Clock, Shutdown};
use super::runtime::{JoinReport, Threads};
use super::shared::handle::{self, clamp_refresh};
pub use super::shared::handle::{Busy, OnChange};
use super::shared::thread::{run_guarded, spawn_ticker, stop_threads, DROP_JOIN};

/// The refresh a driver makes when [`Config::refresh`] is left alone.
pub const DEFAULT_REFRESH: Duration = Duration::from_secs(30);
/// The shortest refresh a driver accepts (a shorter one is raised to this).
pub const MIN_REFRESH: Duration = Duration::from_secs(5);
/// The longest refresh a driver accepts (a longer one is lowered to this): the
/// age rule (rows older than twice the refresh are marked failed) would
/// otherwise mean nothing.
pub const MAX_REFRESH: Duration = Duration::from_secs(300);
/// The room in the worker's inbox.
const INBOX: usize = 64;

/// The refresh a driver uses: `refresh` held between [`MIN_REFRESH`] and
/// `ceiling` ([`MAX_REFRESH`] but in a test).
fn effective_refresh(refresh: Duration, ceiling: Duration) -> Duration {
    clamp_refresh(refresh, MIN_REFRESH, ceiling)
}

pub struct Config {
    /// Load again this often after an answer (held between [`MIN_REFRESH`] and
    /// [`MAX_REFRESH`]). Not optional: it is the only way a job changed at the
    /// terminal shows up, and rows older than twice this are marked failed.
    pub refresh: Duration,
    pub on_change: Option<OnChange>,
}

impl Default for Config {
    fn default() -> Self {
        Config {
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
    alive: Arc<AtomicBool>,
}

impl Handle {
    /// The page as of now: not loaded until the first answer comes.
    pub fn snapshot(&self) -> AutomationsView {
        self.store.view()
    }

    /// Load the list (and the open history) again as soon as the pace allows
    /// (one call of a route a second at most, and any number of requests before
    /// it starts are one). A call that is already out does not satisfy it. A
    /// failing route is tried at once instead of after its backoff wait.
    pub fn refresh(&self) -> Result<(), Busy> {
        self.send(Input::Refresh)
    }

    /// The owner used the switch of the job `id` (its exact id, as
    /// `AutomationId::as_str` gives it) to make it `enabled`. At most one pause
    /// or resume is sent for it, once, and only when the row is not already in
    /// that state, the list is known and the model allows it; the row changes
    /// when the serve answers, and the outcome (a refusal, a failure) is in the
    /// view. A second press while a call is out is ignored, not queued. `Busy`
    /// when the inbox is full or the driver stopped: nothing was sent, press
    /// again.
    pub fn set_enabled(&self, id: &str, enabled: bool) -> Result<(), Busy> {
        self.send(Input::SetEnabled(id.to_owned(), enabled))
    }

    /// Show the history of the automation `id` (a row of the page). Selecting
    /// the open one reads it again; an id that is not a row selects nothing.
    pub fn select(&self, id: &str) -> Result<(), Busy> {
        self.send(Input::Select(Some(id.to_owned())))
    }

    /// Close the history.
    pub fn close_history(&self) -> Result<(), Busy> {
        self.send(Input::Select(None))
    }

    /// Dismiss the notice under the table.
    pub fn dismiss_notice(&self) -> Result<(), Busy> {
        self.send(Input::DismissNotice)
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
pub struct AutomationsDriver {
    handle: Handle,
    stop: Shutdown,
    threads: Threads,
}

impl AutomationsDriver {
    /// Start the threads. The first thing the worker does is ask for the list;
    /// until it answers the view is not loaded (no rows). `Err` only when the
    /// system cannot start a thread (nothing is left running then).
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
        let (jobs_tx, jobs_rx) = sync_channel(1);
        let (runs_tx, runs_rx) = sync_channel(1);
        let (toggle_tx, toggle_rx) = sync_channel(1);
        let handle = Handle {
            store: Arc::clone(&store),
            inbox: inbox.clone(),
            alive: Arc::clone(&alive),
        };
        let mut driver = AutomationsDriver {
            handle,
            stop: stop.clone(),
            threads: Threads::new(),
        };
        let started = (|| {
            let t = &mut driver.threads;
            let worker = Worker {
                engine: Engine::new(Arc::clone(&store), effective_refresh(cfg.refresh, ceiling)),
                inbox: inbox_rx,
                jobs: jobs_tx,
                runs: runs_tx,
                toggle: toggle_tx,
                clock: Arc::clone(&clock),
                stop: stop.clone(),
                store: Arc::clone(&store),
                alive,
                on_change: cfg.on_change,
            };
            t.spawn("automations-worker", move || worker.run())?;
            let (a, i, s) = (Arc::clone(&api), inbox.clone(), stop.clone());
            t.spawn("automations-jobs", move || {
                run_guarded(&i.clone(), || jobs_loader(a, jobs_rx, i, s))
            })?;
            let (a, i, s) = (Arc::clone(&api), inbox.clone(), stop.clone());
            t.spawn("automations-runs", move || {
                run_guarded(&i.clone(), || runs_loader(a, runs_rx, i, s))
            })?;
            let (a, i, s) = (Arc::clone(&api), inbox.clone(), stop.clone());
            t.spawn("automations-toggle", move || {
                run_guarded(&i.clone(), || toggler(a, toggle_rx, i, s))
            })?;
            let (i, c, s) = (inbox.clone(), Arc::clone(&clock), stop.clone());
            spawn_ticker(t, "automations-tick", i, c, s, TICK_EVERY)?;
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

    /// The UI's side. Clones stay usable after the driver stops: they read a
    /// page that is not loaded and refuse commands.
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

impl Drop for AutomationsDriver {
    fn drop(&mut self) {
        self.stop(DROP_JOIN);
    }
}
