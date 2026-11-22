//! The skills driver: keeps the model's `SkillsPageState` (the Skills page and
//! the Connected tools page) true to a serve, read-only.
//!
//! It connects the adapter's pure functions (`adapter::skills`: the bounded
//! mapping and `LoadIds`) to the blocking client. It loads `GET /skills` and
//! `GET /tools` when it starts, again on a slow timer
//! ([`Config::refresh`], 30 s by default) and whenever the owner asks
//! ([`Handle::refresh`]), retries a failed call with a backoff for ever, and
//! hands the UI a plain-data [`SkillsPageView`]. The rules are in `engine`
//! (`Engine::step`), which has no thread and no I/O and is what the tests drive.
//!
//! Nothing here approves, rejects, archives or restores: the model has no such
//! command (approving a skill or a tool is a terminal command, `tole skills
//! approve` and `tole plugin approve`, never a serve route), so the driver has
//! no write side.
//!
//! # Thread model
//!
//! Four threads, all started by [`SkillsDriver::start`], all stopped by
//! [`SkillsDriver::shutdown`]:
//!
//! | thread | does | blocks on |
//! |---|---|---|
//! | `worker` (1) | reads one bounded inbox, runs `Engine::step`, hands the effects to the loaders, tells the UI | the inbox |
//! | `loader` (1 per route) | `GET /skills`, `GET /tools`, one call at a time each | the call |
//! | `ticker` (1) | wakes the worker once a second | the clock |
//!
//! Only the worker changes the state, and it does so one input at a time. The
//! state sits behind a mutex that is held for the apply and nothing else (never
//! across a call or a send), and the UI's callback runs after it is released,
//! so [`Handle::snapshot`] from the UI thread waits at most that long. A call
//! that blocks blocks its own loader, not the worker nor the other route. The
//! inbox is bounded (`INBOX`): a loader waits when it is full (an answer is
//! never dropped), the ticker drops its message, and the UI gets [`Busy`]. A
//! call is bounded by the client (see [`ClientApi`](super::api::ClientApi): 30 s
//! per read and per body, so about a minute at worst).
//!
//! # Panics
//!
//! A panic inside an `Api` call is caught where the call is made and is that
//! call's failure (the page shows the tab as failed, the engine retries); the
//! thread goes on. A panic anywhere else in a thread is reported to the worker,
//! which ends the driver: it stops the other threads and clears the state, so
//! the view reads not loaded (badges `Unknown`, no rows) and
//! [`Handle::is_running`] is false.
//!
//! # Stopping
//!
//! `shutdown` raises one stop signal (it wakes every sleep) and joins the
//! threads for up to a timeout. A loader inside a call is not interrupted (the
//! client has no cancel): it is reported as `detached`, ends when the call
//! returns, and sends nothing the worker will read. When the worker ends it
//! clears the state, so a view read afterwards is never a stale list.
//!
//! # When the lists are known
//!
//! A tab shows rows only after a call of its route succeeded, and takes them
//! back (the tab fails, the badge reads `Unknown`) when:
//!
//! - its next call fails, whatever the reason (404, 429, no connection, a
//!   panic): the model never draws a list that may be out of date as the
//!   current trust state, so the rows go and the failure is shown;
//! - the rows are older than twice the refresh interval (calls that hang);
//! - the machine slept, or the wall clock was moved ahead of the monotonic one
//!   by more than a few seconds: the rows may be hours old, and the call that
//!   was out is ignored.
//!
//! A `notes` line in an answer ("skills not read", "drafts not read", "archive
//! not read", "plugins not read", or a count cut by the adapter's caps) makes
//! the tab partial, which the model reads as `Unknown`, never as "nothing to
//! review". A page that shows a count is therefore showing "as of at most about
//! one refresh interval ago", and should say so. While a refresh is out the
//! rows stay and [`Feed::loading`] is true.
//!
//! # Limits
//!
//! - A skill approved at the terminal shows up at the next refresh (30 s by
//!   default, or at once with [`Handle::refresh`]); there is no event for it.
//! - A hung call holds its route's slot until it returns (the client bounds it
//!   at about a minute), so a refresh after a loss waits for it.

mod engine;
mod store;
mod threads;
mod view;
mod worker;

#[cfg(test)]
mod engine_tests;
#[cfg(test)]
mod fixtures;

use std::io;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{sync_channel, SyncSender, TrySendError};
use std::sync::Arc;
use std::time::Duration;

use tole_desktop_model::Tab;

use self::engine::{Engine, Input, Route};
pub use self::store::Feed;
use self::store::Store;
use self::threads::{loader, run_guarded, ticker};
pub use self::view::SkillsPageView;
use self::worker::Worker;
use super::api::Api;
use super::clock::{Clock, Shutdown};
use super::runtime::{JoinReport, Threads};

/// The refresh a driver makes when [`Config::refresh`] is left alone.
pub const DEFAULT_REFRESH: Duration = Duration::from_secs(30);
/// The shortest refresh a driver accepts (a shorter one is raised to this).
pub const MIN_REFRESH: Duration = Duration::from_secs(5);
/// The longest refresh a driver accepts (a longer one is lowered to this): the
/// age rule (rows older than twice the refresh are not shown) would otherwise
/// mean nothing.
pub const MAX_REFRESH: Duration = Duration::from_secs(300);
/// How long dropping a driver waits for its threads.
const DROP_JOIN: Duration = Duration::from_millis(500);
/// The room in the worker's inbox.
const INBOX: usize = 64;

/// The refresh a driver uses: `refresh` held between [`MIN_REFRESH`] and
/// `ceiling` ([`MAX_REFRESH`] but in a test).
fn effective_refresh(refresh: Duration, ceiling: Duration) -> Duration {
    refresh.clamp(MIN_REFRESH, ceiling.max(MIN_REFRESH))
}

/// What the UI calls when the view changed; runs on the worker thread, with no
/// lock held, so it should only ask the UI to repaint (and read
/// [`Handle::snapshot`] there).
pub type OnChange = Arc<dyn Fn() + Send + Sync>;

pub struct Config {
    /// Load both tabs again this often after an answer (held between
    /// [`MIN_REFRESH`] and [`MAX_REFRESH`]). Not optional: it is the only way a
    /// skill or tool approved at the terminal shows up, and rows older than
    /// twice this are no longer shown.
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

/// The driver is stopped, or its inbox is full: the call did nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Busy;

/// The UI's side of the driver: cheap to clone, usable from any thread.
#[derive(Clone)]
pub struct Handle {
    store: Arc<Store>,
    inbox: SyncSender<Input>,
    alive: Arc<AtomicBool>,
}

impl Handle {
    /// The page as of now: not loaded until the first answers come.
    pub fn snapshot(&self) -> SkillsPageView {
        self.store.view()
    }

    /// Load both tabs again as soon as the pace allows (one call of a route a
    /// second at most, and any number of requests before it starts are one).
    /// A call that is already out does not satisfy it. A failing route is tried
    /// at once instead of after its backoff wait. The outcome shows in the view.
    pub fn refresh(&self) -> Result<(), Busy> {
        self.send(Input::Refresh)
    }

    pub fn select_tab(&self, tab: Tab) -> Result<(), Busy> {
        self.send(Input::SelectTab(tab))
    }

    /// Whether the worker is running.
    pub fn is_running(&self) -> bool {
        self.alive.load(Ordering::SeqCst)
    }

    fn send(&self, input: Input) -> Result<(), Busy> {
        match self.inbox.try_send(input) {
            Ok(()) => Ok(()),
            Err(TrySendError::Full(_) | TrySendError::Disconnected(_)) => Err(Busy),
        }
    }
}

/// The running driver: owns its threads.
pub struct SkillsDriver {
    handle: Handle,
    stop: Shutdown,
    threads: Threads,
}

impl SkillsDriver {
    /// Start the threads. The first thing the worker does is ask for both
    /// routes; until they answer the view is not loaded (`Unknown` badges).
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
        let (skills_tx, skills_rx) = sync_channel(1);
        let (tools_tx, tools_rx) = sync_channel(1);
        let handle = Handle {
            store: Arc::clone(&store),
            inbox: inbox.clone(),
            alive: Arc::clone(&alive),
        };
        let mut driver = SkillsDriver {
            handle,
            stop: stop.clone(),
            threads: Threads::new(),
        };
        let started = (|| {
            let t = &mut driver.threads;
            let worker = Worker {
                engine: Engine::new(Arc::clone(&store), effective_refresh(cfg.refresh, ceiling)),
                inbox: inbox_rx,
                skills: skills_tx,
                tools: tools_tx,
                clock: Arc::clone(&clock),
                stop: stop.clone(),
                store: Arc::clone(&store),
                alive,
                on_change: cfg.on_change,
            };
            t.spawn("skills-worker", move || worker.run())?;
            for (name, route, jobs) in [
                ("skills-load", Route::Skills, skills_rx),
                ("tools-load", Route::Tools, tools_rx),
            ] {
                let (a, i, s) = (Arc::clone(&api), inbox.clone(), stop.clone());
                t.spawn(name, move || {
                    run_guarded(&i.clone(), || loader(a, route, jobs, i, s))
                })?;
            }
            let (i, c, s) = (inbox.clone(), Arc::clone(&clock), stop.clone());
            t.spawn("skills-tick", move || {
                run_guarded(&i.clone(), || ticker(i, c, s))
            })?;
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
    /// a page that is not loaded and refuse commands.
    pub fn handle(&self) -> Handle {
        self.handle.clone()
    }

    /// Stop every thread and wait up to `timeout` for them.
    pub fn shutdown(mut self, timeout: Duration) -> JoinReport {
        self.stop(timeout)
    }

    fn stop(&mut self, timeout: Duration) -> JoinReport {
        self.stop.trigger();
        let _ = self.handle.inbox.try_send(Input::Wake);
        self.threads.join(timeout)
    }
}

impl Drop for SkillsDriver {
    fn drop(&mut self) {
        self.stop(DROP_JOIN);
    }
}
