//! The answers of the two read routes applied to the page.

use tole_client::{ApprovalList, CronJobs, CronRuns, Error};
use tole_desktop_model::{printable, AutomationId, AutomationsState};

use super::{jobs, runs};

/// Longest failure text kept (the model's own limit for a failed load).
const MESSAGE_MAX: usize = 200;

/// What the list answer did to the page.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JobsApplied {
    /// The rows were replaced. `left_out` jobs of the answer are not among them
    /// (see [`Jobs::left_out`](super::Jobs::left_out)).
    Ready { left_out: usize },
    /// The call failed. The old rows stay on the page, marked as failed
    /// (`page.load` is `Failed` with the reason), and the scheduler is no
    /// longer assumed to run.
    Failed,
}

/// The result of `Client::cron_jobs` into the page: the answer replaces the
/// rows (a selection that is gone is dropped with its history, by the model),
/// an error marks the page as failed and says why.
pub fn apply_jobs(page: &mut AutomationsState, result: &Result<CronJobs, Error>) -> JobsApplied {
    match result {
        Ok(list) => {
            let jobs = jobs(list);
            page.loaded(jobs.rows, jobs.scheduler);
            JobsApplied::Ready {
                left_out: jobs.left_out,
            }
        }
        Err(error) => {
            page.load_failed(&error.to_string());
            JobsApplied::Failed
        }
    }
}

/// What the history answer did to the page.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RunsApplied {
    /// `page.history` was replaced (see [`Runs`](super::Runs) for the two
    /// facts that qualify it).
    Ready {
        more_at_least: usize,
        links_known: bool,
    },
    /// The call failed; the history on the page is what it was, and is not
    /// current. The text is safe to draw.
    Failed(String),
}

/// The result of `Client::cron_runs` for `job` into the page, with the pending
/// approvals when the caller read them (`lookup`: `None` when no run needed
/// them, so none were asked for). `None` when `job` is not the selected
/// automation: the answer is for a page that is gone and changes nothing.
pub fn apply_runs(
    page: &mut AutomationsState,
    job: &AutomationId,
    result: &Result<CronRuns, Error>,
    lookup: Option<&Result<ApprovalList, Error>>,
) -> Option<RunsApplied> {
    if page.selected.as_ref() != Some(job) {
        return None;
    }
    Some(match result {
        Ok(list) => {
            let pending = lookup.and_then(|r| r.as_ref().ok());
            let history = runs(job.as_str(), list, pending);
            page.history_loaded(job, history.rows);
            RunsApplied::Ready {
                more_at_least: history.more_at_least,
                links_known: history.links_known,
            }
        }
        Err(error) => RunsApplied::Failed(printable(&error.to_string(), MESSAGE_MAX)),
    })
}
