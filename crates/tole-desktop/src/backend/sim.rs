//! The engine without threads: a test feeds looks and answers by hand, with a
//! clock it moves itself.

use tole_desktop_model::ServeState;

use super::engine::{Effect, Engine, Input, Observation, Out};
use crate::driver::shared::now::Now;

const T0: u64 = 1_700_000_000_000;

/// An engine, the time the test says it is, and what the link and the drivers
/// show.
pub(super) struct Sim {
    pub e: Engine,
    pub now: Now,
    pub ready: bool,
    /// The state the link shows, when the test cares which: otherwise a ready
    /// serve is `Running` and one that is not is `Starting`.
    pub state: Option<ServeState>,
    pub epoch: u64,
    /// The approvals and skills drivers run.
    pub alive: bool,
    /// The task drivers that ended by themselves.
    pub dead: Vec<String>,
    /// The open tasks.
    pub wanted: Vec<String>,
}

pub(super) fn names(ids: &[&str]) -> Vec<String> {
    ids.iter().map(|s| (*s).to_owned()).collect()
}

impl Sim {
    pub(super) fn new() -> Sim {
        Sim {
            e: Engine::new(),
            now: Now { wall: T0, mono: 0 },
            ready: true,
            state: None,
            epoch: 1,
            alive: true,
            dead: Vec::new(),
            wanted: Vec::new(),
        }
    }

    pub(super) fn advance(&mut self, ms: u64) {
        self.now.wall += ms;
        self.now.mono += ms;
    }

    pub(super) fn feed(&mut self, input: Input) -> Out {
        self.e.step(input, self.now)
    }

    pub(super) fn look(&mut self) -> Out {
        let state = self.state.clone().unwrap_or_else(|| {
            if self.ready {
                ServeState::Running {
                    version: "1".into(),
                }
            } else {
                ServeState::Starting
            }
        });
        let obs = Observation {
            state,
            epoch: self.epoch,
            drivers_alive: self.alive,
            dead_tasks: self.dead.clone(),
            wanted: self.wanted.clone(),
        };
        self.feed(Input::Observe(obs))
    }

    /// Look; if the engine asks to attach, answer with `ok`, and answer every
    /// task start with `tasks_ok`. Returns every effect it asked for, in order.
    pub(super) fn settle_with(&mut self, ok: bool, tasks_ok: bool) -> Vec<Effect> {
        let mut effects = Vec::new();
        let out = self.look();
        let attached = out
            .effects
            .iter()
            .any(|e| matches!(e, Effect::Attach { .. }));
        effects.extend(out.effects);
        if attached {
            let out = self.feed(Input::Attached(ok));
            effects.extend(out.effects);
        }
        let starts: Vec<String> = (effects.iter())
            .filter_map(|e| match e {
                Effect::StartTask(id) => Some(id.clone()),
                _ => None,
            })
            .collect();
        for id in starts {
            self.feed(Input::TaskStarted(id, tasks_ok));
        }
        effects
    }

    pub(super) fn settle(&mut self, ok: bool) -> Vec<Effect> {
        self.settle_with(ok, true)
    }

    /// An attached engine (no open task).
    pub(super) fn attached() -> Sim {
        let mut sim = Sim::new();
        let first = sim.settle(true);
        assert_eq!(first, [attach(1, &[])]);
        sim
    }

    /// Whether the engine asks to attach now (and fails it).
    pub(super) fn attach_asked(&mut self) -> bool {
        self.settle(false)
            .iter()
            .any(|e| matches!(e, Effect::Attach { .. }))
    }
}

pub(super) fn attach(epoch: u64, sessions: &[&str]) -> Effect {
    Effect::Attach {
        epoch,
        sessions: names(sessions),
    }
}
