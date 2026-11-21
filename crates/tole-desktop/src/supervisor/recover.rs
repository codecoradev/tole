//! Noticing that the serve stopped and starting it again with a backoff.

use std::time::{Duration, Instant};

use tole_desktop_model::{ServeFailure, ServeState};

use super::engine::{describe_exit, Engine, Outcome};
use super::probe::{self, Probe};

impl Engine {
    /// One look at the serve, from the monitor. A serve that stopped on its
    /// own becomes `Restarting` and is started again once the backoff has
    /// passed. Returns how long the monitor may sleep before the next look
    /// (never more than it asks for), or `None` for "the usual interval".
    pub fn tick(&mut self) -> Option<Duration> {
        match self.hub.state() {
            ServeState::Running { .. } | ServeState::Attached { .. } => {
                if let Some(why) = self.stopped_by_itself() {
                    self.on_stopped(why);
                }
            }
            ServeState::Restarting { .. }
                if self.retry_at.is_some_and(|at| Instant::now() >= at) =>
            {
                self.retry();
            }
            _ => {}
        }
        self.retry_at
            .map(|at| at.saturating_duration_since(Instant::now()))
    }

    /// Why the serve in use is gone, if it is. A child is asked with `wait`; a
    /// serve that was already running is asked on its socket.
    fn stopped_by_itself(&mut self) -> Option<String> {
        match self.owned.as_mut() {
            Some(owned) => {
                let status = owned.serve.try_wait().ok().flatten()?;
                std::thread::sleep(Duration::from_millis(100)); // let stderr drain
                Some(describe_exit(status, &owned.serve.last_lines()))
            }
            None => matches!(probe::probe(&self.cfg.socket()), Probe::Absent)
                .then(|| "it stopped answering".to_owned()),
        }
    }

    fn on_stopped(&mut self, why: String) {
        // A serve that was not started here, or ran steadily, is not a crash loop.
        let ran_for = self
            .owned
            .take()
            .map_or(Duration::MAX, |o| o.started.elapsed());
        self.attempts = self.cfg.backoff.after_run(self.attempts, ran_for);
        self.hub.set_credential(None);
        self.schedule_retry(why);
    }

    /// Count this stop and either wait for the next start or give up.
    fn schedule_retry(&mut self, detail: String) {
        self.attempts += 1;
        let backoff = self.cfg.backoff;
        if backoff.gives_up(self.attempts) {
            self.retry_at = None;
            self.hub.set(ServeState::Failed(ServeFailure::GaveUp {
                attempts: self.attempts - 1,
                detail,
            }));
            return;
        }
        let delay = backoff.delay(self.attempts);
        self.retry_at = Some(Instant::now() + delay);
        self.hub.set(ServeState::Restarting {
            attempt: self.attempts,
            retry_in: delay,
        });
    }

    /// The backoff has passed: attach to or start a serve. A start that fails
    /// counts as another stop; any other failure (no binary, a directory that
    /// is not private, a protocol mismatch) is not helped by trying again.
    fn retry(&mut self) {
        self.retry_at = None;
        match self.ensure() {
            Outcome::Ready => {}
            Outcome::Failed(ServeFailure::StartFailed { detail }) => self.schedule_retry(detail),
            Outcome::Failed(other) => self.hub.set(ServeState::Failed(other)),
        }
    }

    /// The owner asks to try again: the count starts over.
    pub fn retry_now(&mut self) {
        self.attempts = 0;
        self.retry_at = None;
        if let Outcome::Failed(failure) = self.ensure() {
            self.hub.set(ServeState::Failed(failure));
        }
    }
}
