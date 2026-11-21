//! The caller-side effects the model leaves to the adapter, as pure decisions.
//!
//! Nothing here calls the server or waits: a later effects driver makes the
//! calls and owns the timers, and asks these types what a result means.
//!
//! **After an allow** the resumed turn has no prompt call to wait for. The driver
//! polls `GET /status` every [`RESUME_POLL`] and feeds each answer, with the time
//! since the allow, to [`ResumePolling::status`]; it also calls
//! [`ResumePolling::activity`] when the stream showed the turn alive (text,
//! reasoning, a tool or answer entry, a busy `ready`) and [`ResumePolling::tick`]
//! when a poll is slow to answer. It stops when a step is `finished`, or when the
//! task is no longer `run.busy`.
//!
//! - A busy snapshot is `Ready{busy:true}` and confirms the turn.
//! - A not-busy snapshot is sent as `Ready{busy:false}`; the model ignores it
//!   while the turn is still `Resuming` (serve answers 202 before it starts the
//!   turn) and it ends the turn once activity was seen. The polling finishes on
//!   it only after activity (a busy snapshot, or the stream's).
//! - [`ResumeTimedOut`](TaskEvent::ResumeTimedOut) when [`RESUME_TIMEOUT`] has
//!   passed with no activity, or when no activity was seen and the polling is
//!   lost ([`MAX_POLL_FAILURES`] failed calls in a row). Once activity was seen
//!   the turn is known to run, so failed polls alone never end it (a blip of a
//!   second must not show Idle under a running write): the driver keeps polling,
//!   and a really lost link is `ConnectionLost` from the stream.
//!
//! **A busy that is not this task's own call** (a 409 on a prompt, another face's
//! turn, a Stop that was answered) has nothing to wait for, so [`BusyPolling`]
//! polls `GET /status` until the server is not busy and sends `Ready{busy:false}`.
//!
//! **Stop**: [`stop_step`]. serve's cancel answers `cancelled:true` for any known
//! session, idle or not (an unknown one is a 404), so `cancelled:false` is not
//! to be relied on. `cancelled:true` is [`StopStep::Wait`]: the turn ends by the
//! stream or the prompt call, but with no prompt call of this task outstanding
//! (after a 409) nothing would end `Stopping`, so the driver also runs
//! [`BusyPolling::after_stop`]. `cancelled:false` is `Ready{busy:false}`; a failed
//! call is `TaskState::stop_failed`.

use super::live::status_ready;
use std::time::Duration;
use tole_client::{CancelReply, Error, SessionStatus};
use tole_desktop_model::task::{TaskEvent, TaskState};

/// Pause between two status polls after an allow.
pub const RESUME_POLL: Duration = Duration::from_millis(500);
/// How long an allowed turn may show no sign of running before it ends.
pub const RESUME_TIMEOUT: Duration = Duration::from_secs(5);
/// Failed status calls in a row that count as the polling being lost.
pub const MAX_POLL_FAILURES: u32 = 2;
/// Busy snapshots after a Stop was answered before the Stop counts as not
/// honoured (`RESUME_POLL` apart: about 20 s): Stop is offered again.
pub const MAX_STOP_POLLS: u32 = 40;
/// Failed status calls in a row after which a [`BusyPolling`] slows down.
const MAX_BUSY_FAILURES: u32 = 5;
/// The pace of a [`BusyPolling`] that had to slow down.
pub const SLOW_BUSY_POLL: Duration = Duration::from_secs(5);
/// Further failed calls at the slow pace (about a minute) that end the polling
/// with `ConnectionLost`.
const MAX_SLOW_FAILURES: u32 = 12;

/// What to do after one poll result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResumeStep {
    /// Apply these events, in order.
    pub events: Vec<TaskEvent>,
    /// The polling is over.
    pub finished: bool,
}

impl ResumeStep {
    fn more(events: Vec<TaskEvent>) -> Self {
        ResumeStep {
            events,
            finished: false,
        }
    }

    fn done(events: Vec<TaskEvent>) -> Self {
        ResumeStep {
            events,
            finished: true,
        }
    }
}

/// The decisions of one allowed turn, from the allow on.
#[derive(Debug, Clone, Default)]
pub struct ResumePolling {
    active: bool,
    pub(super) failures: u32,
}

impl ResumePolling {
    pub fn new() -> Self {
        Self::default()
    }

    /// The stream showed the turn running.
    pub fn activity(&mut self) {
        self.active = true;
    }

    /// Time passed (`since_allow`) with no answer to act on.
    pub fn tick(&mut self, since_allow: Duration) -> ResumeStep {
        if !self.active && since_allow >= RESUME_TIMEOUT {
            ResumeStep::done(vec![TaskEvent::ResumeTimedOut])
        } else {
            ResumeStep::more(Vec::new())
        }
    }

    /// The result of one `GET /status` made `since_allow` after the allow.
    pub fn status(
        &mut self,
        result: &Result<SessionStatus, Error>,
        since_allow: Duration,
    ) -> ResumeStep {
        match result {
            Ok(status) => {
                self.failures = 0;
                if status.busy {
                    self.active = true;
                    ResumeStep::more(vec![status_ready(status)])
                } else if self.active {
                    ResumeStep::done(vec![status_ready(status)])
                } else if since_allow >= RESUME_TIMEOUT {
                    ResumeStep::done(vec![TaskEvent::ResumeTimedOut])
                } else {
                    ResumeStep::more(vec![status_ready(status)])
                }
            }
            Err(_) => {
                self.failures = self.failures.saturating_add(1);
                if self.active {
                    ResumeStep::more(Vec::new())
                } else if self.failures >= MAX_POLL_FAILURES {
                    ResumeStep::done(vec![TaskEvent::ResumeTimedOut])
                } else {
                    self.tick(since_allow)
                }
            }
        }
    }
}

/// What to do after one poll of a [`BusyPolling`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BusyStep {
    /// Apply these events, in order.
    pub events: Vec<TaskEvent>,
    /// The polling is over.
    pub finished: bool,
    /// After a Stop: the server stayed busy for [`MAX_STOP_POLLS`] polls, so call
    /// `TaskState::stop_failed` (Stop is offered again). Use [`BusyStep::apply`].
    pub stop_failed: bool,
}

impl BusyStep {
    fn new(events: Vec<TaskEvent>, finished: bool) -> Self {
        BusyStep {
            events,
            finished,
            stop_failed: false,
        }
    }

    /// Do what the step says to the task.
    pub fn apply(self, state: &mut TaskState) {
        self.events.into_iter().for_each(|e| state.apply(e));
        if self.stop_failed {
            state.stop_failed();
        }
    }
}

/// Polls `GET /status` while the server is busy with a turn this task has no call
/// for. The driver rule: while `run.busy` and this polling is not `finished`,
/// call [`BusyPolling::status`] every [`BusyPolling::interval`] (it grows to
/// [`SLOW_BUSY_POLL`] once the polling had to give up its fast pace, and the
/// driver must read it again after every step). Stop when a step is `finished`
/// or the task is no longer `run.busy`. The model never stays `Working` with
/// nothing watching: the polling only finishes on a not-busy snapshot, or on
/// [`TaskEvent::ConnectionLost`] after `MAX_SLOW_FAILURES` failed calls in a row
/// at the slow pace (about a minute: the link is gone, and the model says so).
#[derive(Debug, Clone)]
pub struct BusyPolling {
    after_stop: bool,
    busy_polls: u32,
    failures: u32,
    slow: bool,
}

impl BusyPolling {
    /// After a 409 or any busy the task did not start: a busy snapshot is
    /// `Ready{busy:true}` and goes on (the other turn really runs); a not-busy
    /// one is `Ready{busy:false}` and ends it. After `MAX_BUSY_FAILURES` (5)
    /// failed calls in a row the pace becomes slow, it does not stop.
    pub fn foreign() -> Self {
        BusyPolling {
            after_stop: false,
            busy_polls: 0,
            failures: 0,
            slow: false,
        }
    }

    /// After `StopStep::Wait`: as `foreign`, and when the server stays busy for
    /// [`MAX_STOP_POLLS`] snapshots, or the status calls fail
    /// `MAX_BUSY_FAILURES` times in a row, the step says `stop_failed` (Stop is
    /// offered again; the next Stop starts a new polling) and the watch goes on
    /// at the slow pace, so a turn that ends later still ends the task.
    pub fn after_stop() -> Self {
        BusyPolling {
            after_stop: true,
            ..Self::foreign()
        }
    }

    /// How long to wait before the next poll.
    pub fn interval(&self) -> Duration {
        if self.slow {
            SLOW_BUSY_POLL
        } else {
            RESUME_POLL
        }
    }

    fn go_slow(&mut self, step: &mut BusyStep) {
        if !self.slow {
            self.slow = true;
            step.stop_failed = self.after_stop;
        }
    }

    pub fn status(&mut self, result: &Result<SessionStatus, Error>) -> BusyStep {
        let Ok(status) = result else {
            self.failures = self.failures.saturating_add(1);
            let mut step = BusyStep::new(Vec::new(), false);
            if self.failures >= MAX_BUSY_FAILURES {
                self.go_slow(&mut step);
            }
            if self.slow && self.failures >= MAX_BUSY_FAILURES + MAX_SLOW_FAILURES {
                step.events.push(TaskEvent::ConnectionLost);
                step.finished = true;
            }
            return step;
        };
        self.failures = 0;
        if !status.busy {
            return BusyStep::new(vec![status_ready(status)], true);
        }
        self.busy_polls = self.busy_polls.saturating_add(1);
        let mut step = BusyStep::new(vec![status_ready(status)], false);
        if self.after_stop && self.busy_polls >= MAX_STOP_POLLS {
            self.go_slow(&mut step);
        }
        step
    }
}

/// What a Stop call's result asks for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StopStep {
    /// The server is cancelling: the turn ends through the stream or the prompt
    /// call. Also run [`BusyPolling::after_stop`], for when neither comes.
    Wait,
    /// Nothing was running: apply this `Ready{busy:false}`.
    Idle(TaskEvent),
    /// The call failed: `TaskState::stop_failed`, the turn runs on.
    Failed,
}

pub fn stop_step(result: &Result<CancelReply, Error>) -> StopStep {
    match result {
        Ok(r) if r.cancelled => StopStep::Wait,
        Ok(_) => StopStep::Idle(TaskEvent::Ready { busy: false }),
        Err(_) => StopStep::Failed,
    }
}

impl StopStep {
    /// Do what the step says to the task.
    pub fn apply(self, state: &mut TaskState) {
        match self {
            StopStep::Wait => {}
            StopStep::Idle(event) => state.apply(event),
            StopStep::Failed => state.stop_failed(),
        }
    }
}
