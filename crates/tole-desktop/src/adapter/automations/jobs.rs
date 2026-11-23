//! `GET /cron/jobs` to the model's [`RawJob`] rows and [`Scheduler`].

use std::collections::HashSet;

use tole_client::{CronJob, CronJobs, CronScheduler};
use tole_desktop_model::{RawJob, Scheduler};

use super::text::{bounded, TEXT_MAX};

/// Most jobs kept. A person has a handful of automations; the cap only keeps a
/// broken or hostile answer from becoming a table nobody can scroll.
pub const MAX_JOBS: usize = 500;

/// Longest job id kept, in bytes. The id is the one text that is never cut (it
/// is what pause and resume send back, as one path segment, percent-encoded: up
/// to three bytes of request line per byte of id, and the serve drops a request
/// line over 32 KiB). A real id is a few words; one longer than this can never
/// be switched, so its job is left out and counted, and the bytes of ids the
/// page holds are at most `MAX_JOBS * MAX_ID_BYTES`.
pub const MAX_ID_BYTES: usize = 256;

/// The rows of an answer, the scheduler, and how many jobs are not among the
/// rows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Jobs {
    /// At most [`MAX_JOBS`], in the server's order, no id twice, no id over
    /// [`MAX_ID_BYTES`].
    pub rows: Vec<RawJob>,
    pub scheduler: Scheduler,
    /// Jobs of the answer that are not in `rows`: past the cap, with an id
    /// over [`MAX_ID_BYTES`], or with an id an earlier row already has. The model has no way to say a list is
    /// partial, so the page must: with a count above zero it must not show
    /// "N on, M paused" as if it were all of them.
    pub left_out: usize,
}

/// A `CronJob` as the model's job, field for field except the ones the model
/// has no use for (`mode`). The id is the server's exact text (pause and
/// resume send it back; [`jobs`] leaves out one over [`MAX_ID_BYTES`], this
/// function does not look); every other text is bounded
/// (1,024 characters), and `folder` is `None`: the serve row does not
/// carry the job's working folder.
pub fn raw_job(job: &CronJob) -> RawJob {
    RawJob {
        id: job.id.clone(),
        name: bounded(&job.name, TEXT_MAX),
        schedule: bounded(&job.schedule, TEXT_MAX),
        timezone: bounded(&job.timezone, TEXT_MAX),
        state: bounded(&job.state, TEXT_MAX),
        next_run: job.next_run,
        next: bounded(&job.next, TEXT_MAX),
        last: job.last.as_deref().map(|s| bounded(s, TEXT_MAX)),
        missed: job.missed,
        blocked: job.blocked,
        created_by: bounded(&job.created_by, TEXT_MAX),
        awaiting_review: job.awaiting_review,
        folder: None,
    }
}

/// `scheduler.alive` and `scheduler.host` as the model's [`Scheduler`]. Its
/// `age_seconds` is not used: `alive` is the server's own verdict on it.
pub fn scheduler(scheduler: &CronScheduler) -> Scheduler {
    let host = scheduler.host.as_deref().map(|h| bounded(h, TEXT_MAX));
    Scheduler::new(scheduler.alive, host.as_deref())
}

/// The whole answer, bounded: the first [`MAX_JOBS`] jobs with distinct ids of
/// at most [`MAX_ID_BYTES`] bytes.
///
/// A job with a longer id is left out, not cut: a cut id could name another job,
/// and the request for the full one could never be sent.
/// A second job with an id already seen is left out, not kept: the model finds
/// a job by id, so two rows with one id would make a switch act on a different
/// job than the one under the owner's pointer.
pub fn jobs(list: &CronJobs) -> Jobs {
    let mut seen: HashSet<&str> = HashSet::new();
    let mut rows = Vec::new();
    let mut left_out = 0usize;
    for job in &list.jobs {
        let fits = job.id.len() <= MAX_ID_BYTES;
        if fits && rows.len() < MAX_JOBS && seen.insert(job.id.as_str()) {
            rows.push(raw_job(job));
        } else {
            left_out = left_out.saturating_add(1);
        }
    }
    Jobs {
        rows,
        scheduler: scheduler(&list.scheduler),
        left_out,
    }
}
