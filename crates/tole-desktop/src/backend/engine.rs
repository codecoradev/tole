//! The decisions of the backend, with no thread and no I/O.
//!
//! [`Engine::step`] takes one [`Input`] (what the worker saw of the link and
//! of its drivers, or the result of an effect it carried out) and the two
//! readings of time ([`Now`]), and returns the [`Effect`]s the worker must
//! carry out. The rules:
//!
//! - **Tell the UI** of every change of the serve's state or epoch, drivers or
//!   not: the view shows the state live, so a window with no driver (a failed
//!   serve, a retry, a restart that goes on) still hears of each step.
//! - **Attach** only to a serve that is ready, and at most once per
//!   [`ATTACH_GAP`]: a serve that flaps ready and not ready quickly gets new
//!   drivers at that pace, not at the pace it flaps.
//! - **Detach** at once when the serve stops being ready, when its epoch
//!   changed (it was replaced, and a new one may have a new credential, even
//!   when it came back before anyone looked), or when a driver ended by
//!   itself (a thread died). Every task driver goes with the others.
//! - **Retry** an attach that failed (no [`Api`](crate::driver::Api) yet, no
//!   thread for a driver) after `Backoff` (1 s doubling to 30 s), and a driver
//!   that died the same way. The backoff starts over only after the drivers
//!   have run for [`HEALTHY`], so a driver that dies at once is not restarted
//!   in a loop.
//! - **Tasks.** The open tasks are the app's wish (`wanted`, at most
//!   [`MAX_OPEN_TASKS`]), kept whether or not a serve is there. While attached
//!   the engine makes what runs equal it: a task driver is started for each
//!   open task, stopped for each one closed (before any is started, so the
//!   serve's stream slots are never over-used) and replaced when it died, after
//!   a backoff of that task's own (a task whose driver keeps dying delays no
//!   other task, and its entry goes when the task is closed). The approvals driver follows exactly the open tasks'
//!   sessions: it is started with them, and told when they change.
//!
//! Deadlines are on the monotonic clock.

use std::collections::BTreeMap;
use std::time::Duration;

use tole_desktop_model::ServeState;

use crate::adapter::approvals::{Backoff, RETRY_MAX};
use crate::driver::shared::now::{millis, Now};
use crate::driver::task::MAX_OPEN_TASKS;

/// The shortest time between two attaches.
pub(super) const ATTACH_GAP: Duration = Duration::from_secs(1);
/// How long drivers must run for the backoff to start over.
const HEALTHY: Duration = RETRY_MAX;

/// What the worker saw.
pub(super) enum Input {
    /// A look at the link and at the drivers.
    Observe(Observation),
    /// The result of [`Effect::Attach`]: whether the drivers are running.
    Attached(bool),
    /// The result of [`Effect::StartTask`] for this session.
    TaskStarted(String, bool),
}

/// One look, at every message the worker takes.
pub(super) struct Observation {
    /// What the serve is doing. The view shows it as it is, so any change of
    /// it, with or without drivers, is a change to tell the UI of.
    pub state: ServeState,
    /// The link's epoch.
    pub epoch: u64,
    /// The approvals and skills drivers that are attached still run.
    pub drivers_alive: bool,
    /// The task drivers that ended by themselves.
    pub dead_tasks: Vec<String>,
    /// The open tasks.
    pub wanted: Vec<String>,
}

/// What the worker must do.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum Effect {
    /// Ask the link for an `Api` and start the approvals and skills drivers
    /// under `epoch`, the approvals driver following `sessions`; answer with
    /// [`Input::Attached`].
    Attach { epoch: u64, sessions: Vec<String> },
    /// Stop every driver, the tasks' too, and let the UI read `Unknown`.
    Detach,
    /// Start the task driver of this session; answer with
    /// [`Input::TaskStarted`].
    StartTask(String),
    /// Stop these task drivers, all at once.
    StopTasks(Vec<String>),
    /// The approvals driver follows exactly these sessions from now on.
    SetSessions(Vec<String>),
}

#[derive(Debug, Default, PartialEq, Eq)]
pub(super) struct Out {
    /// Something a view shows changed.
    pub changed: bool,
    pub effects: Vec<Effect>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    /// No drivers.
    Detached,
    /// An [`Effect::Attach`] under this epoch is out.
    Attaching(u64),
    /// Drivers run under this epoch since this monotonic time.
    Attached { epoch: u64, since: u64 },
}

pub(super) struct Engine {
    phase: Phase,
    /// The state and epoch of the last look (none before the first).
    seen: Option<(ServeState, u64)>,
    /// No attach before this monotonic time.
    hold_until: u64,
    backoff: Backoff,
    /// The open tasks, as of the last look.
    wanted: Vec<String>,
    /// The task drivers that run, or are being started.
    running: Vec<String>,
    /// What the approvals driver was last told to follow.
    sessions: Vec<String>,
    /// The open tasks whose driver failed, each with a wait of its own. Only
    /// open tasks are kept (at most [`MAX_OPEN_TASKS`]), so it cannot grow.
    waits: BTreeMap<String, TaskWait>,
}

/// What the engine remembers of a task whose driver died or could not start.
struct TaskWait {
    backoff: Backoff,
    /// No start of this task's driver before this monotonic time.
    hold_until: u64,
    /// When its driver was last started: the backoff starts over when it has
    /// run for [`HEALTHY`] since.
    last_start: u64,
}

impl Engine {
    pub(super) fn new() -> Self {
        Engine {
            phase: Phase::Detached,
            seen: None,
            hold_until: 0,
            backoff: Backoff::new(),
            wanted: Vec::new(),
            running: Vec::new(),
            sessions: Vec::new(),
            waits: BTreeMap::new(),
        }
    }

    pub(super) fn step(&mut self, input: Input, now: Now) -> Out {
        let mut out = Out::default();
        match input {
            Input::Observe(obs) => self.observe(obs, now, &mut out),
            Input::Attached(ok) => self.attached(ok, now, &mut out),
            Input::TaskStarted(id, ok) => self.task_started(&id, ok, now, &mut out),
        }
        out
    }

    fn attached(&mut self, ok: bool, now: Now, out: &mut Out) {
        let Phase::Attaching(epoch) = self.phase else {
            return;
        };
        if ok {
            self.phase = Phase::Attached {
                epoch,
                since: now.mono,
            };
            out.changed = true;
            // The open tasks start with the drivers, not a look later.
            self.reconcile(&[], now, out);
        } else {
            self.phase = Phase::Detached;
            self.sessions.clear();
            self.hold_until = now.mono.saturating_add(millis(self.backoff.failed()));
        }
    }

    fn task_started(&mut self, id: &str, ok: bool, now: Now, out: &mut Out) {
        if ok {
            out.changed = true;
        } else if let Some(i) = self.running.iter().position(|t| t == id) {
            self.running.remove(i);
            self.task_failed(id, now);
        }
    }

    /// The driver of `id` died or could not start: its next start waits one
    /// step longer than the last failure of this task's, and no other task's.
    fn task_failed(&mut self, id: &str, now: Now) {
        let wait = self.waits.entry(id.to_owned()).or_insert_with(|| TaskWait {
            backoff: Backoff::new(),
            hold_until: 0,
            last_start: now.mono,
        });
        wait.hold_until = now.mono.saturating_add(millis(wait.backoff.failed()));
    }

    fn observe(&mut self, obs: Observation, now: Now, out: &mut Out) {
        let ready = obs.state.is_ready();
        let seen = (obs.state.clone(), obs.epoch);
        // The first look only records: the UI reads the view when it starts.
        if self.seen.as_ref().is_some_and(|before| *before != seen) {
            out.changed = true;
        }
        self.seen = Some(seen);
        self.wanted = obs.wanted;
        self.wanted.truncate(MAX_OPEN_TASKS);
        if let Phase::Attached { epoch, since } = self.phase {
            let same = ready && obs.epoch == epoch;
            if same && obs.drivers_alive {
                if now.mono.saturating_sub(since) >= millis(HEALTHY) {
                    self.backoff.succeeded();
                }
                self.reconcile(&obs.dead_tasks, now, out);
            } else {
                // A driver that died under a serve that stayed is a failure;
                // a serve that went away or was replaced is not.
                let wait = if same {
                    self.backoff.failed()
                } else {
                    ATTACH_GAP
                };
                self.hold_until = now.mono.saturating_add(millis(wait.max(ATTACH_GAP)));
                self.phase = Phase::Detached;
                self.running.clear();
                self.sessions.clear();
                out.changed = true;
                out.effects.push(Effect::Detach);
            }
        }
        if self.phase == Phase::Detached && ready && now.mono >= self.hold_until {
            self.phase = Phase::Attaching(obs.epoch);
            self.sessions = self.wanted.clone();
            out.effects.push(Effect::Attach {
                epoch: obs.epoch,
                sessions: self.wanted.clone(),
            });
        }
    }

    /// Make the task drivers and the followed sessions equal the open tasks.
    fn reconcile(&mut self, dead: &[String], now: Now, out: &mut Out) {
        let mut stop: Vec<String> = Vec::new();
        for id in dead {
            if self.running.contains(id) {
                stop.push(id.clone());
                self.task_failed(id, now);
            }
        }
        for id in &self.running {
            if !self.wanted.contains(id) && !stop.contains(id) {
                stop.push(id.clone());
            }
        }
        self.running.retain(|t| !stop.contains(t));
        if !stop.is_empty() {
            out.changed = true;
            out.effects.push(Effect::StopTasks(stop));
        }
        if self.sessions != self.wanted {
            self.sessions = self.wanted.clone();
            out.effects.push(Effect::SetSessions(self.wanted.clone()));
        }
        // A task that was closed is forgotten, with its failures.
        self.waits.retain(|id, _| self.wanted.contains(id));
        for id in self.wanted.clone() {
            let held = self.waits.get(&id).is_some_and(|w| now.mono < w.hold_until);
            if held || self.running.contains(&id) {
                continue;
            }
            if let Some(wait) = self.waits.get_mut(&id) {
                wait.last_start = now.mono;
            }
            self.running.push(id.clone());
            out.effects.push(Effect::StartTask(id));
        }
        // A driver that ran for [`HEALTHY`] since its start has paid its debt.
        let running = &self.running;
        self.waits.retain(|id, w| {
            let ran = now.mono.saturating_sub(w.last_start) >= millis(HEALTHY);
            !(ran && running.contains(id) && !dead.contains(id))
        });
    }
}
