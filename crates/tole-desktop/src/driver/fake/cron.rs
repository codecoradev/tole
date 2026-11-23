//! The scripting of the automations calls of the fake serve: the cron jobs,
//! the runs of a job, pause and resume. The trait methods are in the parent.

use std::sync::mpsc::{channel, Sender};

use tole_client::{CronJobState, CronJobs, CronRuns, Result};

use super::{FakeApi, Scripted};

// Part of the scripting surface every driver's tests share.
#[allow(dead_code)]
impl FakeApi {
    scripted!(
        cron_jobs,
        CronJobs,
        reply_cron_jobs,
        gate_cron_jobs,
        panic_cron_jobs
    );
    scripted!(
        cron_runs,
        CronRuns,
        reply_cron_runs,
        gate_cron_runs,
        panic_cron_runs
    );
    scripted!(pauses, CronJobState, reply_pause, gate_pause, panic_pause);
    scripted!(
        resumes,
        CronJobState,
        reply_resume,
        gate_resume,
        panic_resume
    );

    /// How many `GET /cron/jobs` calls were made.
    pub(crate) fn cron_jobs_calls(&self) -> usize {
        self.script().cron_jobs_calls
    }

    /// `(job, limit)` of every `GET /cron/runs` call.
    pub(crate) fn cron_runs_calls(&self) -> Vec<(String, usize)> {
        self.script().cron_runs_calls.clone()
    }

    /// The id of every pause call.
    pub(crate) fn pauses(&self) -> Vec<String> {
        self.script().pause_calls.clone()
    }

    /// The id of every resume call.
    pub(crate) fn resumes(&self) -> Vec<String> {
        self.script().resume_calls.clone()
    }

    pub(crate) fn wait_cron_jobs(&self, n: usize) {
        self.wait("GET /cron/jobs calls", n, |s| s.cron_jobs_calls);
    }

    pub(crate) fn wait_cron_runs(&self, n: usize) {
        self.wait("GET /cron/runs calls", n, |s| s.cron_runs_calls.len());
    }

    pub(crate) fn wait_pauses(&self, n: usize) {
        self.wait("pause calls", n, |s| s.pause_calls.len());
    }

    pub(crate) fn wait_resumes(&self, n: usize) {
        self.wait("resume calls", n, |s| s.resume_calls.len());
    }
}
