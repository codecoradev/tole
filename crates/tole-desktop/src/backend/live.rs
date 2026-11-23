//! The drivers of one attachment to the serve, owned by the worker.

use std::io;
use std::sync::Arc;
use std::time::Duration;

use super::reap::{self, Job};
use super::shared::{Attached, Fanin};
use crate::driver::api::Api;
use crate::driver::approvals::{self, ApprovalsDriver};
use crate::driver::clock::Clock;
use crate::driver::runtime::JoinReport;
use crate::driver::skills::{self, SkillsDriver};

/// What the backend's [`Config`](super::Config) says about the drivers.
#[derive(Clone, Copy)]
pub(super) struct Settings {
    pub approvals_refresh: Duration,
    pub skills_refresh: Duration,
}

/// The drivers that run over one `Api`, and the handles the UI reads them by.
pub(super) struct Live {
    approvals: ApprovalsDriver,
    skills: SkillsDriver,
    attached: Arc<Attached>,
}

impl Live {
    /// Start the approvals and the skills drivers over `api`. `Err` when the
    /// system cannot start a thread; nothing is left running then.
    pub(super) fn start(
        api: Arc<dyn Api>,
        clock: &Arc<dyn Clock>,
        epoch: u64,
        settings: Settings,
        fan: &Arc<Fanin>,
    ) -> io::Result<Live> {
        let approvals = ApprovalsDriver::start(
            Arc::clone(&api),
            Arc::clone(clock),
            approvals::Config {
                refresh: settings.approvals_refresh,
                on_change: Some(fan.callback()),
                ..approvals::Config::default()
            },
        )?;
        let skills = SkillsDriver::start(
            api,
            Arc::clone(clock),
            skills::Config {
                refresh: settings.skills_refresh,
                on_change: Some(fan.callback()),
            },
        )?;
        let attached = Arc::new(Attached {
            epoch,
            approvals: approvals.handle(),
            skills: skills.handle(),
        });
        Ok(Live {
            approvals,
            skills,
            attached,
        })
    }

    /// What the UI reads the drivers by.
    pub(super) fn attached(&self) -> Arc<Attached> {
        Arc::clone(&self.attached)
    }

    /// Every driver still runs: none ended by itself (a thread died).
    pub(super) fn alive(&self) -> bool {
        self.attached.approvals.is_running() && self.attached.skills.is_running()
    }

    /// Stop every driver at once, each waiting up to `timeout` for its threads.
    pub(super) fn shutdown(self, timeout: Duration) -> JoinReport {
        let Live {
            approvals, skills, ..
        } = self;
        let jobs: Vec<Job> = vec![
            Box::new(move || approvals.shutdown(timeout)),
            Box::new(move || skills.shutdown(timeout)),
        ];
        reap::all(jobs)
    }
}
