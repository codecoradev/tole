//! The decisions of the automations driver, with no thread and no I/O.
//!
//! [`Engine::step`] takes one [`Input`] (an answer, a tick, an owner press) and
//! the two readings of time ([`Now`]), applies it to the model through the
//! adapter, and returns the [`Effect`]s the threads must carry out (call
//! `GET /cron/jobs`, call `GET /cron/runs`, pause or resume a job). The rules:
//!
//! - **Two reads, two schedules.** The job list and the history of the
//!   selected automation load from their own routes with their own pace and
//!   backoff (`Lane`), so one that fails does not hold back the other. At most
//!   one call of each is out. The history is loaded only while an automation is
//!   selected, and again whenever the selection changes.
//! - **First load, refresh, pace, failure, age, sleep.** As the skills driver:
//!   at once from `start`; again [`Config::refresh`](super::Config::refresh)
//!   after an answer and whenever the owner asks; at most one call of a route a
//!   second; a failed call is retried after 1 s doubling to 30 s, for ever; a
//!   list that could not be read, or is older than twice the refresh, or that
//!   the machine slept through, keeps its rows but is marked failed
//!   (`page.load`), and the scheduler is then not assumed to run.
//! - **A switch is one call, answered once.** `Input::SetEnabled` asks the
//!   model for the request (`AutomationsState::request_toggle`: nothing is sent
//!   for a job awaiting the owner's review, for a plan-mode server that refuses
//!   resumes, for a switch already in the wanted state, or while another call
//!   is out), and the pause or resume is sent once and never repeated, whatever
//!   comes back. The row changes only when the serve answers, to the state the
//!   serve names: no optimistic flip. A switch is not sent while the list is not
//!   known (`page.load` is not `Ready`): the owner would be switching a row the
//!   page cannot vouch for.
//! - **A switch racing a read.** Whatever the answer to a switch was (done,
//!   refused, an error that may or may not have reached the serve), the list is
//!   due again at once, and a list call that is out is no longer trusted: it
//!   may have read before the switch took effect, and applied it would put the
//!   old state back under the new one. Its answer frees the call slot and
//!   changes nothing.
//!
//! Deadlines are on the monotonic clock; the wall clock is read only to see the
//! jump.

mod jobs;
mod runs;
mod toggling;

use std::sync::Arc;
use std::time::Duration;

use tole_client::{ApprovalList, CronJobState, CronJobs, CronRuns, Error};
use tole_desktop_model::{AutomationId, ToggleRequest};

use super::store::Store;
use crate::driver::shared::lane::Lane;
use crate::driver::shared::thread::DriverInput;

/// The worker wakes at least this often (a refresh, a backoff or the age of the
/// rows can fall due while the inbox is silent).
pub(super) const TICK_EVERY: Duration = Duration::from_secs(1);

/// A wall clock that moves further than the monotonic one by more than this
/// between two steps was suspended or corrected: what was loaded is lost.
const JUMP_GAP_MS: u64 = 5_000;

/// What the page says when it takes rows back for being too old.
pub(super) const STALE: &str = "Not refreshed in time: the list may be out of date";

pub(super) use crate::driver::shared::now::{millis, Now};

/// The identity of one call of any route of the driver. They never repeat: after
/// `u64::MAX` calls (never reached in practice) none is given, nothing new
/// starts and the page keeps its last state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct CallId(u64);

#[derive(Debug, Default)]
struct Calls(u64);

impl Calls {
    fn next(&mut self) -> Option<CallId> {
        self.0 = self.0.checked_add(1)?;
        Some(CallId(self.0))
    }
}

/// The answer of a history call: the runs of one automation, and the pending
/// approvals when some run needed them.
pub(super) struct RunsReply {
    pub job: AutomationId,
    pub runs: Result<CronRuns, Error>,
    /// `None` when no run needed the approval list, so none was asked for.
    pub lookup: Option<Result<ApprovalList, Error>>,
}

/// What reaches the engine.
pub(super) enum Input {
    /// The answer of the list call the engine asked for with this id.
    Jobs(CallId, Result<CronJobs, Error>),
    /// The answer of the history call with this id.
    Runs(CallId, RunsReply),
    /// The answer of the pause or resume with this id.
    Toggled(CallId, ToggleRequest, Result<CronJobState, Error>),
    /// The owner asked to load now.
    Refresh,
    /// The owner used the switch of the job with this id (its exact text).
    SetEnabled(String, bool),
    /// Show the history of this automation, or close it.
    Select(Option<String>),
    DismissNotice,
    /// The clock moved on (the worker also keeps its own time).
    Tick,
    /// Nothing to do: wakes the worker to look at its stop signal.
    Wake,
    /// A driver thread died by a panic outside any call: the driver ends.
    ThreadFailed,
}

/// What a thread must do for the engine.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum Effect {
    /// Call `GET /cron/jobs` and answer with [`Input::Jobs`] and this id.
    LoadJobs(CallId),
    /// Call `GET /cron/runs` of this job and answer with [`Input::Runs`].
    LoadRuns(CallId, AutomationId),
    /// Send this pause or resume once and answer with [`Input::Toggled`].
    Toggle(CallId, ToggleRequest),
}

#[derive(Default)]
pub(super) struct Out {
    /// Something a view shows changed.
    pub changed: bool,
    pub effects: Vec<Effect>,
    /// The driver must end (a thread is gone).
    pub fatal: bool,
}

impl DriverInput for Input {
    fn tick() -> Self {
        Input::Tick
    }
    fn wake() -> Self {
        Input::Wake
    }
    fn thread_failed() -> Self {
        Input::ThreadFailed
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Route {
    Jobs,
    Runs,
}

pub(super) struct Engine {
    store: Arc<Store>,
    calls: Calls,
    refresh: Duration,
    jobs: Lane<CallId>,
    runs: Lane<CallId>,
    /// The switch call that is out, if any: the only answer of a switch that is
    /// applied to the page.
    toggle: Option<CallId>,
    /// The time of the last step, to see the clock jump.
    last_seen: Option<Now>,
}

impl Engine {
    pub(super) fn new(store: Arc<Store>, refresh: Duration) -> Self {
        Engine {
            store,
            calls: Calls::default(),
            refresh,
            jobs: Lane::new(),
            runs: Lane::new(),
            toggle: None,
            last_seen: None,
        }
    }

    /// The first step: nothing is loaded, the list is asked for at once.
    pub(super) fn start(&mut self, now: Now) -> Out {
        self.step(Input::Wake, now)
    }

    /// One input.
    pub(super) fn step(&mut self, input: Input, now: Now) -> Out {
        let mut out = Out::default();
        if self.store.take_reset() {
            // The state was thrown away: nothing is loaded, and what is out
            // describes a page that is gone (a switch answer too).
            out.changed = true;
            self.toggle = None;
            self.lose_all(true);
        }
        out.changed |= self.check_jump(now);
        out.changed |= match input {
            Input::Jobs(id, result) => self.jobs_loaded(id, result, now),
            Input::Runs(id, reply) => self.runs_loaded(id, reply, now),
            Input::Toggled(id, request, result) => self.toggled(id, request, result),
            Input::Refresh => {
                self.jobs.asap = true;
                self.runs.asap = true;
                false
            }
            Input::SetEnabled(id, enabled) => self.set_enabled(&id, enabled, &mut out),
            Input::Select(id) => self.select(id),
            Input::DismissNotice => {
                let mut shared = self.store.lock();
                let had = shared.page.notice.is_some();
                shared.page.dismiss_notice();
                had
            }
            Input::ThreadFailed => {
                out.fatal = true;
                false
            }
            Input::Tick | Input::Wake => false,
        };
        out.changed |= self.housekeeping(now, &mut out);
        out
    }

    fn lane(&mut self, route: Route) -> &mut Lane<CallId> {
        match route {
            Route::Jobs => &mut self.jobs,
            Route::Runs => &mut self.runs,
        }
    }

    fn lose(&mut self, route: Route, drop_call: bool) -> bool {
        match route {
            Route::Jobs => self.lose_jobs(drop_call),
            Route::Runs => self.lose_runs(drop_call),
        }
    }

    fn lose_all(&mut self, drop_call: bool) -> bool {
        [Route::Jobs, Route::Runs]
            .into_iter()
            .fold(false, |c, r| self.lose(r, drop_call) | c)
    }

    /// The wall clock moved further than the monotonic one: the machine slept
    /// (the monotonic clock stops in a suspend) or the clock was corrected
    /// forward. Not scheduling: only this detection reads the wall delta.
    fn check_jump(&mut self, now: Now) -> bool {
        let Some(prev) = self.last_seen.replace(now) else {
            return false;
        };
        let wall = now.wall.saturating_sub(prev.wall);
        let mono = now.mono.saturating_sub(prev.mono);
        wall > mono.saturating_add(JUMP_GAP_MS) && self.lose_all(true)
    }

    /// What is due after every input: data that is too old, the refreshes and
    /// retries that fell due, and the calls that may start.
    fn housekeeping(&mut self, now: Now, out: &mut Out) -> bool {
        let limit = millis(self.refresh).saturating_mul(2);
        let mut changed = false;
        for route in [Route::Jobs, Route::Runs] {
            if self.lane(route).aged(now.mono, limit) {
                changed |= self.lose(route, false);
            }
            self.lane(route).schedule(now.mono);
            changed |= match route {
                Route::Jobs => self.begin_jobs(now, out),
                Route::Runs => self.begin_runs(now, out),
            };
        }
        changed
    }
}

#[cfg(test)]
mod input_mapping {
    use super::{DriverInput, Input};

    /// A wrong mapping would compile and silently stop a thread panic from
    /// ending the driver, so the three are pinned.
    #[test]
    fn the_shared_thread_bodies_get_the_inputs_they_mean() {
        assert!(matches!(<Input as DriverInput>::tick(), Input::Tick));
        assert!(matches!(<Input as DriverInput>::wake(), Input::Wake));
        assert!(matches!(
            <Input as DriverInput>::thread_failed(),
            Input::ThreadFailed
        ));
    }
}
