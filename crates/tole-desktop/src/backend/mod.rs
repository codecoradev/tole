//! The backend: the module that keeps the drivers true to the local serve.
//!
//! The window needs one thing to read and one thing to call: this. It owns the
//! approvals, skills, automations and task drivers ([`approvals`], [`skills`],
//! [`automations`], [`task`]) and
//! starts and stops them as the serve comes and goes, so the window never holds
//! a driver's handle that a restart of the serve would orphan.
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
//! backend.open_task("20261123-1")?; // a driver follows it, now or when the serve is ready
//! let _ = backend.task_view("20261123-1"); // None while the serve is down
//! let _ = backend.send("20261123-1", "hello"); // Err(Unavailable) then, never lost silently
//! backend.shutdown(Duration::from_secs(1)); // the serve itself is left alone
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```
//!
//! # What it does
//!
//! - While the serve is ready ([`ServeState::is_ready`](tole_desktop_model::ServeState::is_ready))
//!   it keeps the approvals, skills and automations drivers running over one
//!   [`Api`](crate::driver::Api).
//! - When the serve stops being ready (restarting, failed, stopping, stopped,
//!   another protocol version), or is replaced, it stops them, and the view
//!   reads `Unknown` from that moment. When the serve is ready again it builds
//!   a new `Api` (a new serve may have a new credential) and starts new
//!   drivers.
//! - The app opens and closes tasks by session id ([`Backend::open_task`],
//!   [`Backend::close_task`], at most [`MAX_OPEN_TASKS`](task::MAX_OPEN_TASKS),
//!   the fourth is refused with a reason). The open set is the app's wish and
//!   survives the serve: a task opened while the serve is down is started when
//!   it is ready, and after a restart the drivers are made again for the same
//!   sessions. The approvals driver follows exactly the open sessions. Task
//!   commands go through the backend by session id ([`Backend::send`],
//!   [`Backend::steer`], [`Backend::stop`], [`Backend::decide`]) and the view of
//!   a task is [`Backend::task_view`], so the UI never holds a handle that a
//!   restart would orphan. A command is queued on the driver that is there now
//!   or refused with a reason ([`TaskRefused`]); it is never kept for the next
//!   driver, and what the old driver had queued when the serve went away is
//!   lost (the task driver's own rule: a UI keeps the words it typed until the
//!   transcript shows them).
//! - It never flaps: at most one attach per second, and an attach that cannot
//!   build an `Api` is retried after 1 s, doubling to 30 s (see `engine`).
//!
//! # The shell
//!
//! [`shell::apply`] is a pure function that applies a [`BackendView`] to the
//! model's `ShellState`: the sidebar counts and the connection dot. An unknown
//! count is never written as a zero (see its docs for the skills count, which the
//! shell cannot yet say is unknown).
//!
//! # Thread model
//!
//! | thread | does | blocks on |
//! |---|---|---|
//! | `worker` (1) | reads the link at every message, runs `Engine::step`, starts and stops the drivers, tells the UI | its inbox |
//! | `ticker` (1) | wakes the worker every 100 ms of the clock | the clock |
//! | `reap` (1 per driver, only while detaching) | stops a driver, so several stop at once | the driver's join |
//!
//! The drivers' own threads are theirs (see their module docs): the approvals
//! driver's four fixed threads and up to four readers, the skills driver's four,
//! the automations driver's five, and a task driver's nine for each open task. The stream budget is in `tasks`: at most
//! six of the serve's eight event streams are live (three tasks and the
//! approvals readers of the same sessions), seven by the constants.
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

#[cfg(test)]
mod e2e_tasks_tests;
#[cfg(test)]
mod e2e_tests;
mod engine;
#[cfg(test)]
mod engine_tasks_tests;
#[cfg(test)]
mod engine_tests;
#[cfg(test)]
mod fake;
#[cfg(test)]
mod hostile_tests;
mod link;
#[cfg(test)]
mod link_tests;
mod live;
mod new_task;
#[cfg(test)]
mod new_task_tests;
mod reap;
#[cfg(test)]
mod rig;
mod shared;
pub mod shell;
#[cfg(test)]
mod shell_tests;
#[cfg(test)]
mod sim;
mod tasks;
#[cfg(test)]
mod units_tests;
mod view;
mod worker;

use std::io;
use std::sync::mpsc::{sync_channel, SyncSender};
use std::sync::Arc;
use std::time::Duration;

use tole_desktop_model::approvals::Decision;

use self::engine::Engine;
use self::link::Link;
pub use self::link::{ServeLink, SupervisorLink};
use self::live::Settings;
pub use self::new_task::NewTaskRefused;
use self::shared::{Fanin, Msg, Shared};
pub use self::tasks::{OpenRefused, TaskRefused};
pub use self::view::BackendView;
use self::worker::Worker;
use crate::driver::approvals::{self, ApprovalsView};
use crate::driver::automations::{self, AutomationsView};
use crate::driver::clock::{Clock, Shutdown};
use crate::driver::runtime::{JoinReport, Threads};
use crate::driver::shared::handle::{Busy, OnChange};
use crate::driver::shared::thread::{spawn_ticker, stop_threads};
use crate::driver::skills::{self, SkillsPageView};
use crate::driver::task::{self, TaskView};

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
    /// The automations driver's refresh (held between its own bounds).
    pub automations_refresh: Duration,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            on_change: None,
            approvals_refresh: approvals::DEFAULT_REFRESH,
            skills_refresh: skills::DEFAULT_REFRESH,
            automations_refresh: automations::DEFAULT_REFRESH,
        }
    }
}

/// The running backend: owns its threads, and through them the drivers.
pub struct Backend {
    shared: Arc<Shared>,
    link: Link,
    fan: Arc<Fanin>,
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
            fan: Arc::clone(&fan),
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
                    automations_refresh: cfg.automations_refresh,
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
                backend.end(DROP_JOIN);
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
            automations: attached
                .as_ref()
                .map_or_else(AutomationsView::not_loaded, |a| a.automations.snapshot()),
            serve: look.state,
            open_tasks: self.shared.tasks.list(),
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

    /// [`with_approvals`](Self::with_approvals) for the automations driver
    /// (switch, select, refresh, dismiss).
    pub fn with_automations<R>(&self, f: impl FnOnce(&automations::Handle) -> R) -> Option<R> {
        let look = self.link.look();
        let attached = self.shared.current(&look)?;
        Some(f(&attached.automations))
    }

    /// Record `session` as an open task: a driver follows it from now on (when
    /// the serve is ready, otherwise as soon as it is), and the approvals driver
    /// follows it too. Opening a task that is open is `Ok` and takes no slot.
    ///
    /// Refused ([`OpenRefused`]) for an id the serve cannot have given
    /// (`[A-Za-z0-9_-]`, 1 to 64 bytes) and when [`MAX_OPEN_TASKS`] tasks are
    /// open: each holds one of the serve's eight event streams (see the
    /// "stream budget" in `tasks`), so the UI says so and offers to close one.
    /// The task opens with its history read from the serve; a plan-mode session
    /// is not told apart yet (`task::Config::look_only` is not set).
    ///
    /// [`MAX_OPEN_TASKS`]: crate::driver::task::MAX_OPEN_TASKS
    pub fn open_task(&self, session: &str) -> Result<(), OpenRefused> {
        if !self.is_running() {
            return Err(OpenRefused::Stopped);
        }
        if self.shared.tasks.open(session)? {
            self.changed();
        }
        Ok(())
    }

    /// Stop following `session` (its driver stops, the approvals driver
    /// follows one less). `true` when it was open. A task closed while the serve
    /// is down is not started when the serve is ready.
    pub fn close_task(&self, session: &str) -> bool {
        let was_open = self.shared.tasks.close(session);
        if was_open {
            self.changed();
        }
        was_open
    }

    /// An open task, as of now: `None` when it is not open, or has no driver
    /// (the serve is not ready, was replaced, or the driver is being made
    /// again), never a view of a driver that stopped.
    pub fn task_view(&self, session: &str) -> Option<TaskView> {
        self.with_task(session, |h| h.snapshot())
    }

    /// Call `f` with the handle of an open task's driver (viewport, row
    /// heights, a group toggle, and the commands the methods below wrap), if
    /// the task has a driver for the serve that is there now. The handle lives
    /// for the call only: a restart replaces the driver, and a handle kept
    /// would read stopped for good.
    pub fn with_task<R>(&self, session: &str, f: impl FnOnce(&task::Handle) -> R) -> Option<R> {
        if !self.shared.tasks.contains(session) {
            return None;
        }
        let look = self.link.look();
        let attached = self.shared.current(&look)?;
        let (_, handle) = attached.tasks.iter().find(|(id, _)| id == session)?;
        Some(f(handle))
    }

    /// [`task::Handle::send`] on an open task.
    pub fn send(&self, session: &str, text: &str) -> Result<(), TaskRefused> {
        self.command(session, |h| h.send(text))
    }

    /// [`task::Handle::steer`] on an open task.
    pub fn steer(&self, session: &str, text: &str) -> Result<(), TaskRefused> {
        self.command(session, |h| h.steer(text))
    }

    /// [`task::Handle::stop`] on an open task: stops the running turn, not the
    /// task (that is [`close_task`](Self::close_task)).
    pub fn stop(&self, session: &str) -> Result<(), TaskRefused> {
        self.command(session, task::Handle::stop)
    }

    /// [`task::Handle::decide`] on an open task.
    pub fn decide(
        &self,
        session: &str,
        approval: &str,
        decision: Decision,
    ) -> Result<(), TaskRefused> {
        self.command(session, |h| h.decide(approval, decision))
    }

    /// A command is queued on the driver that is there now, or refused with
    /// the reason: it is never held for a later driver (a restart drops what
    /// the old one had queued, and a UI keeps the words it typed until the
    /// transcript shows them).
    fn command(
        &self,
        session: &str,
        f: impl FnOnce(&task::Handle) -> Result<(), Busy>,
    ) -> Result<(), TaskRefused> {
        if !self.shared.tasks.contains(session) {
            return Err(TaskRefused::NotOpen);
        }
        match self.with_task(session, f) {
            None => Err(TaskRefused::Unavailable),
            Some(Err(Busy)) => Err(TaskRefused::Busy),
            Some(Ok(())) => Ok(()),
        }
    }

    /// The view changed: tell the UI (at the backend's pace) and have the
    /// worker look.
    fn changed(&self) {
        self.fan.mark();
        let _ = self.inbox.try_send(Msg::Wake);
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
        self.end(timeout)
    }

    fn end(&mut self, timeout: Duration) -> JoinReport {
        let own = stop_threads(&self.stop, &self.inbox, &mut self.threads, timeout);
        reap::sum(own, self.shared.totals())
    }
}

impl Drop for Backend {
    fn drop(&mut self) {
        self.end(DROP_JOIN);
    }
}
