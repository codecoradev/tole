//! What the tests of the automations driver share: answers built from the
//! server's own JSON (the adapter's fixtures), and the engine on a clock the
//! test moves by hand.

use std::sync::Arc;
use std::time::Duration;

use tole_client::{CronJobState, CronJobs, CronRuns, Error};
use tole_desktop_model::{AutomationId, AutomationsState, Load, Notice};

use super::engine::{CallId, Effect, Engine, Input, Now, Out, RunsReply};
use super::store::Store;
use super::view::AutomationsView;
pub(super) use crate::adapter::automations::fixtures::*;

pub(super) const T0: u64 = 1_700_000_000_500;
pub(super) const REFRESH: Duration = Duration::from_secs(30);

pub(super) fn id(s: &str) -> AutomationId {
    AutomationId::new(s)
}

/// An answer with a job the owner has on (`on`), one paused (`off`) and one
/// Tole made that nobody reviewed (`tole`).
pub(super) fn three() -> CronJobs {
    jobs_answer(vec![
        job_json("on", "cli", "active", false),
        job_json("off", "cli", "paused", false),
        job_json("tole", "agent", "paused", true),
    ])
}

pub(super) fn jobs_reply(list: CronJobs) -> Result<CronJobs, Error> {
    Ok(list)
}

/// The answer to a pause or a resume of `job`.
pub(super) fn toggled_to(job: &str, state: &str) -> Result<CronJobState, Error> {
    Ok(job_state(job, state))
}

pub(super) fn runs_of(job: &str, statuses: &[&str]) -> CronRuns {
    runs_answer(
        statuses
            .iter()
            .enumerate()
            .map(|(i, s)| run_json(&format!("f{i}"), job, Some(s), None))
            .collect(),
    )
}

pub(super) fn runs_reply(job: &str, runs: CronRuns) -> RunsReply {
    RunsReply {
        job: id(job),
        runs: Ok(runs),
        lookup: None,
    }
}

/// The list call the output asks for, if any.
pub(super) fn load_of(out: &Out) -> Option<CallId> {
    out.effects.iter().find_map(|e| match e {
        Effect::LoadJobs(id) => Some(*id),
        _ => None,
    })
}

/// The history call the output asks for, if any.
pub(super) fn runs_call_of(out: &Out) -> Option<(CallId, AutomationId)> {
    out.effects.iter().find_map(|e| match e {
        Effect::LoadRuns(id, job) => Some((*id, job.clone())),
        _ => None,
    })
}

/// The switch call the output asks for, if any.
pub(super) fn toggle_of(out: &Out) -> Option<(CallId, tole_desktop_model::ToggleRequest)> {
    out.effects.iter().find_map(|e| match e {
        Effect::Toggle(id, req) => Some((*id, req.clone())),
        _ => None,
    })
}

/// An engine, its store and the time the test says it is.
pub(super) struct T {
    pub e: Engine,
    pub store: Arc<Store>,
    pub now: Now,
}

impl T {
    pub(super) fn new() -> T {
        T::with_refresh(REFRESH)
    }

    pub(super) fn with_refresh(refresh: Duration) -> T {
        let store = Arc::new(Store::default());
        T {
            e: Engine::new(Arc::clone(&store), refresh),
            store,
            now: Now {
                wall: T0,
                mono: 1_000,
            },
        }
    }

    /// Started and the list answered: `on`, `off` and `tole`.
    pub(super) fn known() -> T {
        let mut t = T::new();
        let out = t.start();
        t.step(Input::Jobs(
            load_of(&out).expect("the first call"),
            jobs_reply(three()),
        ));
        assert_eq!(t.view().page.load, Load::Ready);
        t
    }

    pub(super) fn start(&mut self) -> Out {
        self.e.start(self.now)
    }

    pub(super) fn step(&mut self, input: Input) -> Out {
        self.e.step(input, self.now)
    }

    /// Both readings move by `ms`, then the engine hears a tick.
    pub(super) fn after(&mut self, ms: u64) -> Out {
        self.now.wall += ms;
        self.now.mono += ms;
        self.step(Input::Tick)
    }

    /// The wall clock alone moves (the machine slept, the clock was set).
    pub(super) fn jump(&mut self, ms: u64) -> Out {
        self.now.wall += ms;
        self.step(Input::Tick)
    }

    /// Move time in quarter seconds until a list call is made, at most `ms`;
    /// the call and the time it took.
    pub(super) fn next_load(&mut self, ms: u64) -> Option<(CallId, u64)> {
        let mut spent = 0;
        while spent < ms {
            spent += 250;
            if let Some(id) = load_of(&self.after(250)) {
                return Some((id, spent));
            }
        }
        None
    }

    /// The same for the history call.
    pub(super) fn next_runs(&mut self, ms: u64) -> Option<(CallId, u64)> {
        let mut spent = 0;
        while spent < ms {
            spent += 250;
            if let Some((id, _)) = runs_call_of(&self.after(250)) {
                return Some((id, spent));
            }
        }
        None
    }

    pub(super) fn view(&self) -> AutomationsView {
        self.store.view()
    }

    pub(super) fn page(&self) -> AutomationsState {
        self.view().page
    }

    pub(super) fn notice(&self) -> Option<Notice> {
        self.view().page.notice
    }

    /// The owner presses the switch of `job` to make it `enabled`.
    pub(super) fn press(&mut self, job: &str, enabled: bool) -> Out {
        self.step(Input::SetEnabled(job.to_owned(), enabled))
    }

    /// The owner opens the history of `job` and the first call is answered.
    pub(super) fn open(&mut self, job: &str, statuses: &[&str]) -> CallId {
        let out = self.step(Input::Select(Some(job.to_owned())));
        let (call, of) = runs_call_of(&out).expect("a history call");
        assert_eq!(of, id(job));
        self.step(Input::Runs(call, runs_reply(job, runs_of(job, statuses))));
        call
    }
}
