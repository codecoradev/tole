//! The drivers of one attachment to the serve, owned by the worker.

use std::io;
use std::sync::Arc;
use std::time::Duration;

use super::reap::{self, Job};
use super::shared::{Attached, Fanin};
use crate::driver::api::Api;
use crate::driver::approvals::{self, ApprovalsDriver};
use crate::driver::automations::{self, AutomationsDriver};
use crate::driver::clock::Clock;
use crate::driver::runtime::JoinReport;
use crate::driver::skills::{self, SkillsDriver};
use crate::driver::task::{self, TaskDriver};

/// What the backend's [`Config`](super::Config) says about the drivers.
#[derive(Clone, Copy)]
pub(super) struct Settings {
    pub approvals_refresh: Duration,
    pub skills_refresh: Duration,
    pub automations_refresh: Duration,
}

/// The drivers that run over one `Api`, and the handles the UI reads them by.
pub(super) struct Live {
    api: Arc<dyn Api>,
    clock: Arc<dyn Clock>,
    fan: Arc<Fanin>,
    epoch: u64,
    approvals: ApprovalsDriver,
    skills: SkillsDriver,
    automations: AutomationsDriver,
    tasks: Vec<(String, TaskDriver)>,
    attached: Arc<Attached>,
}

impl Live {
    /// Start the approvals driver, following `sessions`, and the skills and
    /// automations drivers over `api`. `Err` when the system cannot start a thread; nothing is left
    /// running then.
    pub(super) fn start(
        api: Arc<dyn Api>,
        clock: &Arc<dyn Clock>,
        epoch: u64,
        settings: Settings,
        fan: &Arc<Fanin>,
        sessions: Vec<String>,
    ) -> io::Result<Live> {
        let approvals = ApprovalsDriver::start(
            Arc::clone(&api),
            Arc::clone(clock),
            approvals::Config {
                sessions,
                refresh: settings.approvals_refresh,
                on_change: Some(fan.callback()),
            },
        )?;
        let skills = SkillsDriver::start(
            Arc::clone(&api),
            Arc::clone(clock),
            skills::Config {
                refresh: settings.skills_refresh,
                on_change: Some(fan.callback()),
            },
        )?;
        let automations = AutomationsDriver::start(
            Arc::clone(&api),
            Arc::clone(clock),
            automations::Config {
                refresh: settings.automations_refresh,
                on_change: Some(fan.callback()),
            },
        )?;
        let attached = Arc::new(Attached {
            epoch,
            api: Arc::clone(&api),
            approvals: approvals.handle(),
            skills: skills.handle(),
            automations: automations.handle(),
            tasks: Vec::new(),
        });
        Ok(Live {
            api,
            clock: Arc::clone(clock),
            fan: Arc::clone(fan),
            epoch,
            attached,
            approvals,
            skills,
            automations,
            tasks: Vec::new(),
        })
    }

    /// What the UI reads the drivers by.
    pub(super) fn attached(&self) -> Arc<Attached> {
        Arc::clone(&self.attached)
    }

    fn rebuild(&mut self) {
        self.attached = Arc::new(Attached {
            epoch: self.epoch,
            api: Arc::clone(&self.api),
            approvals: self.approvals.handle(),
            skills: self.skills.handle(),
            automations: self.automations.handle(),
            tasks: (self.tasks.iter())
                .map(|(id, driver)| (id.clone(), driver.handle()))
                .collect(),
        });
    }

    /// The approvals, skills and automations drivers still run: none ended by
    /// itself (a thread died).
    pub(super) fn alive(&self) -> bool {
        self.attached.approvals.is_running()
            && self.attached.skills.is_running()
            && self.attached.automations.is_running()
    }

    /// The tasks whose driver ended by itself.
    pub(super) fn dead_tasks(&self) -> Vec<String> {
        (self.attached.tasks.iter())
            .filter(|(_, handle)| !handle.is_running())
            .map(|(id, _)| id.clone())
            .collect()
    }

    /// Start the driver of `session`. `Err` when the system cannot start a
    /// thread (nothing is left running then). A session that has a driver
    /// already is left alone.
    pub(super) fn start_task(&mut self, session: &str) -> io::Result<()> {
        if self.tasks.iter().any(|(id, _)| id == session) {
            return Ok(());
        }
        let mut cfg = task::Config::new(session);
        cfg.on_change = Some(self.fan.callback());
        let driver = TaskDriver::start(Arc::clone(&self.api), Arc::clone(&self.clock), cfg)?;
        self.tasks.push((session.to_owned(), driver));
        self.rebuild();
        Ok(())
    }

    /// Take the drivers of these sessions out of the set (the handles the UI
    /// reads stop listing them); the caller stops them with [`stop_all`].
    pub(super) fn take_tasks(&mut self, sessions: &[String]) -> Vec<TaskDriver> {
        let (gone, kept): (Vec<_>, Vec<_>) = std::mem::take(&mut self.tasks)
            .into_iter()
            .partition(|(id, _)| sessions.contains(id));
        self.tasks = kept;
        self.rebuild();
        gone.into_iter().map(|(_, driver)| driver).collect()
    }

    /// The approvals driver follows these sessions from now on. A driver that
    /// stopped refuses; the next look finds it dead.
    pub(super) fn set_sessions(&self, sessions: Vec<String>) {
        let _ = self.attached.approvals.set_sessions(sessions);
    }

    /// Stop every driver at once, each waiting up to `timeout` for its threads.
    pub(super) fn shutdown(self, timeout: Duration) -> JoinReport {
        let Live {
            approvals,
            skills,
            automations,
            tasks,
            ..
        } = self;
        let mut jobs: Vec<Job> = vec![
            Box::new(move || approvals.shutdown(timeout)),
            Box::new(move || skills.shutdown(timeout)),
            Box::new(move || automations.shutdown(timeout)),
        ];
        for (_, driver) in tasks {
            jobs.push(Box::new(move || driver.shutdown(timeout)));
        }
        reap::all(jobs)
    }
}

/// Stop these task drivers at once, each waiting up to `timeout` for its threads.
pub(super) fn stop_all(drivers: Vec<TaskDriver>, timeout: Duration) -> JoinReport {
    let jobs = (drivers.into_iter())
        .map(|driver| -> Job { Box::new(move || driver.shutdown(timeout)) })
        .collect();
    reap::all(jobs)
}
