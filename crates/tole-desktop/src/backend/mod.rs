//! The backend: the module that keeps the drivers true to the local serve.
//!
//! The window needs one thing to read and one thing to call: this. It owns the
//! approvals and skills drivers ([`approvals`], [`skills`]) and starts and
//! stops them as the serve comes and goes, so the window never holds a driver's
//! handle that a restart of the serve would orphan.
//!
//! ```no_run
//! use std::sync::Arc;
//! use std::time::Duration;
//! use tole_desktop::backend::{Backend, Config, SupervisorLink};
//! use tole_desktop::driver::SystemClock;
//! use tole_desktop::supervisor::{default_runtime_dir, Config as Launch};
//!
//! let runtime = default_runtime_dir(None, &std::env::temp_dir(), 1000);
//! let link = Arc::new(SupervisorLink::launch(Launch::new("/home/me/project", runtime)));
//! let backend = Backend::start(link, Arc::new(SystemClock), Config::default())?;
//! let view = backend.snapshot(); // plain data: the serve, the approvals, the skills
//! println!("{:?}", view.serve);
//! backend.shutdown(Duration::from_secs(1)); // the serve itself is left alone
//! # Ok::<(), std::io::Error>(())
//! ```
//!
//! # What it does
//!
//! - While the serve is ready ([`ServeState::is_ready`](tole_desktop_model::ServeState::is_ready))
//!   it keeps the approvals driver and the skills driver running over one
//!   [`Api`](crate::driver::Api).
//! - When the serve stops being ready (restarting, failed, stopping, stopped,
//!   another protocol version), or is replaced, it stops them, and the view
//!   reads `Unknown` from that moment. When the serve is ready again it builds
//!   a new `Api` (a new serve may have a new credential) and starts new
//!   drivers.
//! - It never flaps: at most one attach per second, and an attach that cannot
//!   build an `Api` is retried after 1 s, doubling to 30 s (see `engine`).
//!
//! # Thread model
//!
//! | thread | does | blocks on |
//! |---|---|---|
//! | `worker` (1) | reads the link at every message, runs `Engine::step`, starts and stops the drivers, tells the UI | its inbox |
//! | `ticker` (1) | wakes the worker every 100 ms of the clock | the clock |
//! | `reap` (1 per driver, only while detaching) | stops a driver, so several stop at once | the driver's join |
//!
//! Only the worker starts or stops a driver. The state the UI reads is one
//! `Arc` of handles behind a mutex that is held for a clone and nothing else,
//! so [`Backend::snapshot`] never waits for a driver, a call or the worker.
//! No lock is held while a driver is started or stopped, and the link is
//! never called with one held.
//!
//! The inbox is bounded and carries no state (`Msg`): the worker reads the
//! link and the drivers afresh at every message, so a message that is dropped
//! loses nothing, and stopping is a signal that is never dropped.
//!
//! # The view and the serve that was replaced
//!
//! [`Backend::snapshot`] reads the link itself and shows the drivers only
//! when they were started under the serve that is there now (same epoch, ready):
//! a restart that the worker has not yet caught up with reads `Unknown`, not
//! the old serve's numbers. The link's [`epoch`](ServeLink::epoch) is what
//! catches a restart that came back between two looks.
//!
//! # `on_change`
//!
//! [`Config::on_change`] is called on the worker thread with no lock held, at
//! most once per 100 ms: every driver's `on_change`, every change of the
//! serve's state (also when no driver is attached) and the backend's own
//! changes set one flag, and a change that comes within 100 ms of the last
//! call waits for the next tick. It should only ask the UI to repaint and read
//! [`Backend::snapshot`] there. A panic in it is contained.
//!
//! # Panics
//!
//! The link is called under a guard: a link that panics reads as a serve that
//! is not ready. A driver that ends by itself (a thread died) is noticed and
//! replaced with a growing wait. A panic anywhere else in the worker ends the
//! backend: the drivers are stopped, the view reads `Unknown` and
//! [`Backend::is_running`] is false.
//!
//! # Stopping
//!
//! [`Backend::shutdown`] raises one signal and joins the worker for up to the
//! timeout; the worker stops the drivers (at most 500 ms each, all at once).
//! A thread inside a call or a stream read cannot be interrupted (the client
//! has no cancel), is counted as `detached` in the report and ends by itself;
//! it holds one of the serve's event streams until its next frame (a heartbeat,
//! at most 15 s) or the 45 s idle timeout. The serve is left alone: closing it
//! is the supervisor's `close_window`.

mod engine;
mod link;
mod live;
mod reap;
mod shared;
mod view;
mod worker;

use std::io;
use std::sync::mpsc::{sync_channel, SyncSender};
use std::sync::Arc;
use std::time::Duration;

use self::engine::Engine;
use self::link::Link;
pub use self::link::{ServeLink, SupervisorLink};
use self::live::Settings;
use self::shared::{Fanin, Msg, Shared};
pub use self::view::BackendView;
use self::worker::Worker;
use crate::driver::approvals::{self, ApprovalsView};
use crate::driver::clock::{Clock, Shutdown};
use crate::driver::runtime::{JoinReport, Threads};
use crate::driver::shared::handle::OnChange;
use crate::driver::shared::thread::{spawn_ticker, stop_threads};
use crate::driver::skills::{self, SkillsPageView};

/// The room in the worker's inbox.
const INBOX: usize = 64;
/// How long dropping the backend waits: the worker stops the drivers for up to
/// `worker::DETACH_JOIN`, then ends.
const DROP_JOIN: Duration = Duration::from_secs(1);
/// The worker looks at the serve and at its drivers this often.
const TICK_EVERY: Duration = Duration::from_millis(100);

pub struct Config {
    /// Called when something the view shows changed (see "`on_change`" in the
    /// module docs).
    pub on_change: Option<OnChange>,
    /// The approvals driver's refresh (held between its own bounds).
    pub approvals_refresh: Duration,
    /// The skills driver's refresh (held between its own bounds).
    pub skills_refresh: Duration,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            on_change: None,
            approvals_refresh: approvals::DEFAULT_REFRESH,
            skills_refresh: skills::DEFAULT_REFRESH,
        }
    }
}

/// The running backend: owns its threads, and through them the drivers.
pub struct Backend {
    shared: Arc<Shared>,
    link: Link,
    inbox: SyncSender<Msg>,
    stop: Shutdown,
    threads: Threads,
}

impl Backend {
    /// Start the worker and its ticker. The first thing the worker does is
    /// look at the serve: when it is ready the drivers start at once; until
    /// then (and until their first answers) the view is `Unknown`. `Err` only
    /// when the system cannot start a thread (nothing is left running then).
    pub fn start(
        link: Arc<dyn ServeLink>,
        clock: Arc<dyn Clock>,
        cfg: Config,
    ) -> io::Result<Backend> {
        let link = Link::new(link);
        let shared = Arc::new(Shared::new());
        let stop = Shutdown::new();
        let (inbox, inbox_rx) = sync_channel::<Msg>(INBOX);
        let fan = Fanin::new(inbox.clone());
        let mut backend = Backend {
            shared: Arc::clone(&shared),
            link: link.clone(),
            inbox: inbox.clone(),
            stop: stop.clone(),
            threads: Threads::new(),
        };
        let started = (|| {
            let worker = Worker {
                engine: Engine::new(),
                inbox: inbox_rx,
                link,
                clock: Arc::clone(&clock),
                stop: stop.clone(),
                shared,
                fan,
                settings: Settings {
                    approvals_refresh: cfg.approvals_refresh,
                    skills_refresh: cfg.skills_refresh,
                },
                on_change: cfg.on_change,
                live: None,
                last_notified: None,
            };
            let t = &mut backend.threads;
            t.spawn("backend-worker", move || worker.run())?;
            spawn_ticker(t, "backend-tick", inbox, clock, stop, TICK_EVERY)
        })();
        match started {
            Ok(()) => Ok(backend),
            Err(e) => {
                backend.stop(DROP_JOIN);
                Err(e)
            }
        }
    }

    /// The serve and what the drivers know, as of now. Cheap; safe from any
    /// thread.
    pub fn snapshot(&self) -> BackendView {
        let look = self.link.look();
        let attached = self.shared.current(&look);
        BackendView {
            approvals: attached
                .as_ref()
                .map_or_else(ApprovalsView::unknown, |a| a.approvals.snapshot()),
            skills: attached
                .as_ref()
                .map_or_else(SkillsPageView::not_loaded, |a| a.skills.snapshot()),
            serve: look.state,
        }
    }

    /// Call `f` with the approvals driver's handle (decide, select, tab, the
    /// message box), if there is a driver for the serve that is there now:
    /// `None` otherwise. The handle lives for the call only, so a restart
    /// cannot leave the UI holding one.
    pub fn with_approvals<R>(&self, f: impl FnOnce(&approvals::Handle) -> R) -> Option<R> {
        let look = self.link.look();
        let attached = self.shared.current(&look)?;
        Some(f(&attached.approvals))
    }

    /// [`with_approvals`](Self::with_approvals) for the skills driver (refresh,
    /// tab).
    pub fn with_skills<R>(&self, f: impl FnOnce(&skills::Handle) -> R) -> Option<R> {
        let look = self.link.look();
        let attached = self.shared.current(&look)?;
        Some(f(&attached.skills))
    }

    /// Whether the worker runs. False after [`shutdown`](Self::shutdown) and
    /// when a panic ended it.
    pub fn is_running(&self) -> bool {
        self.shared.is_alive()
    }

    /// What the drivers stopped so far reported (they are stopped when the
    /// serve goes away, is replaced, or the backend ends). `detached` counts
    /// threads that were still inside a call and were let go.
    pub fn thread_report(&self) -> JoinReport {
        self.shared.totals()
    }

    /// Stop the worker and, through it, the drivers; wait up to `timeout`
    /// for them (a timeout under half a second may find the worker still
    /// stopping them, and counts it detached). The report adds up the backend's own
    /// threads and every driver it ever stopped. The serve is left alone.
    pub fn shutdown(mut self, timeout: Duration) -> JoinReport {
        self.stop(timeout)
    }

    fn stop(&mut self, timeout: Duration) -> JoinReport {
        let own = stop_threads(&self.stop, &self.inbox, &mut self.threads, timeout);
        reap::sum(own, self.shared.totals())
    }
}

impl Drop for Backend {
    fn drop(&mut self) {
        self.stop(DROP_JOIN);
    }
}
