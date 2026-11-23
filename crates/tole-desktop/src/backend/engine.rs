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
//!   itself (a thread died).
//! - **Retry** an attach that failed (no [`Api`](crate::driver::Api) yet, no
//!   thread for a driver) after `Backoff` (1 s doubling to 30 s), and a driver
//!   that died the same way. The backoff starts over only after the drivers
//!   have run for [`HEALTHY`], so a driver that dies at once is not restarted
//!   in a loop.
//!
//! Deadlines are on the monotonic clock.

use std::time::Duration;

use tole_desktop_model::ServeState;

use crate::adapter::approvals::{Backoff, RETRY_MAX};
use crate::driver::shared::now::{millis, Now};

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
}

/// One look, at every message the worker takes.
pub(super) struct Observation {
    /// What the serve is doing. The view shows it as it is, so any change of
    /// it, with or without drivers, is a change to tell the UI of.
    pub state: ServeState,
    /// The link's epoch.
    pub epoch: u64,
    /// Every driver that is attached still runs.
    pub drivers_alive: bool,
}

/// What the worker must do.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum Effect {
    /// Ask the link for an `Api` and start the drivers under `epoch`; answer
    /// with [`Input::Attached`].
    Attach { epoch: u64 },
    /// Stop every driver, and let the UI read `Unknown`.
    Detach,
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
}

impl Engine {
    pub(super) fn new() -> Self {
        Engine {
            phase: Phase::Detached,
            seen: None,
            hold_until: 0,
            backoff: Backoff::new(),
        }
    }

    pub(super) fn step(&mut self, input: Input, now: Now) -> Out {
        let mut out = Out::default();
        match input {
            Input::Observe(obs) => self.observe(&obs, now, &mut out),
            Input::Attached(ok) => self.attached(ok, now, &mut out),
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
        } else {
            self.phase = Phase::Detached;
            self.hold_until = now.mono.saturating_add(millis(self.backoff.failed()));
        }
    }

    fn observe(&mut self, obs: &Observation, now: Now, out: &mut Out) {
        let ready = obs.state.is_ready();
        let seen = (obs.state.clone(), obs.epoch);
        // The first look only records: the UI reads the view when it starts.
        if self.seen.as_ref().is_some_and(|before| *before != seen) {
            out.changed = true;
        }
        self.seen = Some(seen);
        if let Phase::Attached { epoch, since } = self.phase {
            let same = ready && obs.epoch == epoch;
            if same && obs.drivers_alive {
                if now.mono.saturating_sub(since) >= millis(HEALTHY) {
                    self.backoff.succeeded();
                }
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
                out.changed = true;
                out.effects.push(Effect::Detach);
            }
        }
        if self.phase == Phase::Detached && ready && now.mono >= self.hold_until {
            self.phase = Phase::Attaching(obs.epoch);
            out.effects.push(Effect::Attach { epoch: obs.epoch });
        }
    }
}
