//! Watching a turn nobody is waiting for: `GET /status` polled until the
//! server says the turn is over.
//!
//! The model is `busy` after an allow (the resumed turn has no prompt call), a
//! 409, a busy snapshot or a Stop that was answered; none of those has an end
//! that is certain to arrive on the stream. So whenever the model is busy and
//! no prompt call of ours is out, one watcher runs (`Mode`), and the adapter's
//! types decide what every answer means:
//!
//! - `Resume` (`ResumePolling`): the model is `resuming` after an allow. Polled
//!   every `RESUME_POLL` until the turn shows a sign of life, then every
//!   `ALIVE_POLL` (the stream is delivering; the poll only catches an end it
//!   missed), and after failed polls with a growing wait up to `MAX_POLL`;
//!   ended by `RESUME_TIMEOUT` with no sign of life,
//!   or by a not-busy snapshot after activity. The stream's own signs of life
//!   (text, a tool or answer entry, a busy `ready`) are fed to it.
//! - `Busy` (`BusyPolling`): `foreign` for any other busy, `after_stop` after a
//!   cancel the serve accepted (it also says when Stop must be offered again).
//!
//! A watcher that ends while the model is still busy (a not-busy snapshot the
//! model ignored, because its prompt had not been seen yet) is replaced after
//! `SLOW_BUSY_POLL`, never at once. An answer is applied only if its token is
//! the current watcher's: a read from before an allow cannot settle the turn
//! that the allow started. A one-shot check (`Check`, after a failed send, a
//! 409 steer or a suspend) reads the status a few times until it answers.

use std::time::Duration;

use tole_client::{Error, SessionStatus};
use tole_desktop_model::task::TaskEvent;

use super::{millis, Engine, Now, Out};
use crate::adapter::task::{status_ready, BusyPolling, ResumePolling, RESUME_POLL, SLOW_BUSY_POLL};

/// The poll of a resumed turn that is known to run (see the module docs).
const ALIVE_POLL: Duration = Duration::from_secs(3);
/// The longest wait between two polls of a resumed turn after failures.
const MAX_POLL: Duration = Duration::from_secs(5);

/// How long to wait before the next poll of a resumed turn.
fn resume_wait(alive: bool, fails: u32) -> Duration {
    let base = if alive { ALIVE_POLL } else { RESUME_POLL };
    let backoff = RESUME_POLL.saturating_mul(1 << fails.min(4));
    if fails == 0 {
        base
    } else {
        base.max(backoff.min(MAX_POLL))
    }
}
use crate::driver::task::calls::{Call, Lane};

/// Tries of a one-shot status check, and the first wait between two of them.
const CHECK_TRIES: u32 = 5;
const CHECK_RETRY: Duration = Duration::from_secs(2);

pub(super) enum Mode {
    Off,
    Resume {
        /// The turn showed a sign of life: the stream or a status says so.
        alive: bool,
        /// Polls that failed in a row.
        fails: u32,
        polling: ResumePolling,
        /// When the allow was seen (monotonic).
        since: u64,
    },
    Busy {
        polling: BusyPolling,
        after_stop: bool,
    },
}

impl Mode {
    pub(super) fn after_stop() -> Mode {
        Mode::Busy {
            polling: BusyPolling::after_stop(),
            after_stop: true,
        }
    }

    fn foreign() -> Mode {
        Mode::Busy {
            polling: BusyPolling::foreign(),
            after_stop: false,
        }
    }
}

struct Check {
    due: u64,
    tries: u32,
    token: u64,
}

pub(super) struct Watching {
    mode: Mode,
    /// The generation of the watcher: bumped whenever one starts or ends.
    token: u64,
    /// Earliest start of the next poll (monotonic).
    poll_at: u64,
    /// When the poll in flight was made: the next is due a wait after it, not
    /// after its answer, so a slow answer does not stretch the pace.
    issued_at: u64,
    /// No new `foreign` watcher before this (monotonic).
    restart_at: u64,
    check: Option<Check>,
    check_ids: u64,
}

impl Default for Watching {
    fn default() -> Self {
        Watching {
            mode: Mode::Off,
            token: 0,
            poll_at: 0,
            issued_at: 0,
            restart_at: 0,
            check: None,
            check_ids: 0,
        }
    }
}

impl Watching {
    pub(super) fn start(&mut self, mode: Mode, now: Now) {
        let wait = match &mode {
            Mode::Busy { polling, .. } => polling.interval(),
            _ => RESUME_POLL,
        };
        self.mode = mode;
        self.token = self.token.wrapping_add(1);
        self.poll_at = now.mono.saturating_add(millis(wait));
    }

    /// Read the status soon, once (with retries): the model may be wrong about
    /// whether a turn runs.
    pub(super) fn check_soon(&mut self, now: Now) {
        if self.check.is_none() {
            self.check = Some(Check {
                due: now.mono,
                tries: 0,
                token: 0,
            });
        }
    }

    pub(super) fn cancel_check(&mut self) {
        self.check = None;
    }

    /// The stream showed the turn alive (see `ResumePolling::activity`).
    pub(super) fn activity(&mut self, events: &[TaskEvent]) {
        if let Mode::Resume { polling, alive, .. } = &mut self.mode {
            let shows_life = |e: &TaskEvent| {
                matches!(
                    e,
                    TaskEvent::TextDelta { .. }
                        | TaskEvent::Reasoning
                        | TaskEvent::ToolIntent { .. }
                        | TaskEvent::ToolSettled { .. }
                        | TaskEvent::AnswerEntry { .. }
                        | TaskEvent::Ready { busy: true }
                )
            };
            if events.iter().any(shows_life) {
                polling.activity();
                *alive = true;
            }
        }
    }
}

impl Engine {
    /// Make the watcher fit the model: none when it is not busy, a resume
    /// watcher after an allow, a foreign one when a turn runs that no call of
    /// ours will report. Returns whether the model changed (it does not).
    pub(super) fn watch_sync(&mut self, now: Now) -> bool {
        let (busy, resuming) = {
            let shared = self.store.lock();
            (shared.task.run.busy, shared.task.run.resuming)
        };
        let w = &mut self.watch;
        if !busy {
            if !matches!(w.mode, Mode::Off) {
                w.mode = Mode::Off;
                w.token = w.token.wrapping_add(1);
            }
        } else if resuming {
            if matches!(
                w.mode,
                Mode::Off
                    | Mode::Busy {
                        after_stop: false,
                        ..
                    }
            ) {
                let mode = Mode::Resume {
                    alive: false,
                    fails: 0,
                    polling: ResumePolling::new(),
                    since: now.mono,
                };
                w.start(mode, now);
            }
        } else if matches!(w.mode, Mode::Off)
            && !self.out_on[Lane::Prompt.index()]
            && now.mono >= w.restart_at
        {
            w.start(Mode::foreign(), now);
        }
        false
    }

    /// The resume timeout runs on time, not on answers: a slow or lost poll
    /// must not keep an unconfirmed turn open.
    pub(super) fn watch_tick(&mut self, now: Now) -> bool {
        let Mode::Resume { polling, since, .. } = &mut self.watch.mode else {
            return false;
        };
        let since = Duration::from_millis(now.mono.saturating_sub(*since));
        let step = polling.tick(since);
        let any = !step.events.is_empty();
        self.apply(step.events);
        if step.finished {
            self.watch_finished(now);
        }
        any
    }

    fn watch_finished(&mut self, now: Now) {
        self.watch.mode = Mode::Off;
        self.watch.token = self.watch.token.wrapping_add(1);
        let still_busy = self.store.lock().task.run.busy;
        self.watch.restart_at = if still_busy {
            now.mono.saturating_add(millis(SLOW_BUSY_POLL))
        } else {
            0
        };
    }

    /// Start the status read that is due: the one-shot check first, then the
    /// watcher's poll.
    pub(super) fn status_next(&mut self, now: Now, out: &mut Out) {
        if self.lane_busy(Lane::Status) {
            return;
        }
        let w = &mut self.watch;
        let call = if let Some(c) = w.check.as_mut().filter(|c| now.mono >= c.due) {
            w.check_ids = w.check_ids.wrapping_add(1);
            (c.token, c.due) = (w.check_ids, u64::MAX);
            Call::Status {
                token: c.token,
                check: true,
            }
        } else if !matches!(w.mode, Mode::Off) && now.mono >= w.poll_at {
            // Not again until the answer is in.
            w.poll_at = u64::MAX;
            w.issued_at = now.mono;
            Call::Status {
                token: w.token,
                check: false,
            }
        } else {
            return;
        };
        self.take_lane(Lane::Status);
        out.effects.push(super::Effect::Call(Lane::Status, call));
    }

    pub(super) fn status_reply(
        &mut self,
        token: u64,
        check: bool,
        reply: Result<SessionStatus, Error>,
        now: Now,
    ) -> bool {
        self.free_lane(Lane::Status);
        if check {
            return self.check_reply(token, reply, now);
        }
        // A poll that was in flight is answered, whatever it said: the next
        // one is due at the watcher's pace (set below, or by a new watcher).
        if self.watch.poll_at == u64::MAX {
            self.watch.poll_at = now.mono;
        }
        if token != self.watch.token {
            return false;
        }
        let (events, finished, wait, stop_failed) = match &mut self.watch.mode {
            Mode::Off => return false,
            Mode::Resume {
                polling,
                since,
                alive,
                fails,
            } => {
                let ago = Duration::from_millis(now.mono.saturating_sub(*since));
                let step = polling.status(&reply, ago);
                *fails = if reply.is_ok() {
                    0
                } else {
                    fails.saturating_add(1)
                };
                *alive |= matches!(&reply, Ok(s) if s.busy);
                (
                    step.events,
                    step.finished,
                    resume_wait(*alive, *fails),
                    false,
                )
            }
            Mode::Busy { polling, .. } => {
                let step = polling.status(&reply);
                (
                    step.events,
                    step.finished,
                    polling.interval(),
                    step.stop_failed,
                )
            }
        };
        let changed = !events.is_empty() || stop_failed;
        {
            let mut shared = self.store.lock();
            events.into_iter().for_each(|e| shared.task.apply(e));
            if stop_failed {
                shared.task.stop_failed();
            }
        }
        if finished {
            self.watch_finished(now);
        } else {
            self.watch.poll_at = self.watch.issued_at.saturating_add(millis(wait));
        }
        changed
    }

    fn check_reply(&mut self, token: u64, reply: Result<SessionStatus, Error>, now: Now) -> bool {
        let Some(c) = self.watch.check.as_mut().filter(|c| c.token == token) else {
            return false;
        };
        match reply {
            Ok(status) => {
                self.watch.check = None;
                self.apply(vec![status_ready(&status)]);
                true
            }
            Err(_) => {
                c.tries += 1;
                if c.tries >= CHECK_TRIES {
                    self.watch.check = None;
                } else {
                    let wait = CHECK_RETRY.saturating_mul(1 << c.tries.min(4));
                    c.due = now.mono.saturating_add(millis(wait));
                }
                false
            }
        }
    }
}
