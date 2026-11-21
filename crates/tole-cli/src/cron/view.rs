//! What a graphical client needs of `tole cron` (#1042, `tole serve`): the
//! jobs as data, and pause and resume.
//!
//! Reading is [`super::list`] and [`status::status`] as JSON. Changing is
//! deliberately narrow: **pause** may stop any job (it only removes
//! authority), and **resume** may only restart a job whose owner already
//! vouched for it. A job an agent saved with the `cron_create` tool runs only
//! after the owner reviewed it and ran `tole cron resume` (`enabled_by = cli`,
//! [`super::trust`]); that review is trust approval, which is never a serve
//! route, so resuming such a job here is refused and names the CLI command.
//! Creating, editing and removing a job stay local-only owner actions.

use std::path::Path;

use anyhow::{bail, Context};
use serde_json::{json, Value};
use tole_core::ledger::Job;

use super::{field, status, store, trust};

/// The start of the refusal [`resume`] gives for a job the owner has not
/// reviewed, so a caller can answer `403`.
pub const NOT_REVIEWED: &str = "not reviewed";

/// `GET /cron/jobs`: every job with its schedule, next run, last result and
/// who made it, and whether a ticker is alive to run them.
pub fn jobs_json(dir: &Path, now: i64) -> anyhow::Result<Value> {
    let jobs: Vec<Value> = super::list(dir)?
        .into_iter()
        .map(|r| {
            json!({
                "id": r.id,
                "name": r.name,
                "schedule": r.schedule,
                "timezone": r.timezone,
                "mode": r.mode,
                "state": r.state,
                "next_run": r.next_run,
                "next": r.next,
                "last": r.last,
                "missed": r.missed,
                "blocked": r.blocked,
                "created_by": r.created_by,
                "awaiting_review": !r.reviewed,
            })
        })
        .collect();
    let st = status::status(dir, now)?;
    Ok(json!({
        "jobs": jobs,
        "scheduler": {
            "alive": st.alive,
            "host": st.ticker.as_ref().map(|t| t.host.clone()),
            "age_seconds": st.age,
        },
    }))
}

/// A job by its id (never by name: an id is what [`jobs_json`] lists).
fn by_id(dir: &Path, id: &str) -> anyhow::Result<Job> {
    let l = store::open_read(dir)?;
    l.state()
        .job(id)
        .filter(|j| !j.job.removed)
        .map(|j| j.job.clone())
        .with_context(|| format!("no job `{id}`"))
}

/// True when the owner stands behind the job: saved through the CLI, or an
/// agent's job the owner reviewed and enabled.
pub fn reviewed(job: &Job) -> bool {
    trust::by_owner(job) || field(job, "enabled_by") == Some(trust::CLI)
}

/// Pauses job `id`. Always allowed: a paused job has less authority.
pub fn pause(dir: &Path, id: &str, now: i64) -> anyhow::Result<Job> {
    by_id(dir, id)?;
    super::update(dir, id, now, |j| j.paused = true)
}

/// Resumes job `id` if its owner vouched for it already. The record of the
/// owner's review (`enabled_by`) is never written here.
pub fn resume(dir: &Path, id: &str, now: i64) -> anyhow::Result<Job> {
    store::with_lock(dir, |l| {
        let mut job = l
            .state()
            .job(id)
            .filter(|j| !j.job.removed)
            .map(|j| j.job.clone())
            .with_context(|| format!("no job `{id}`"))?;
        if !reviewed(&job) {
            bail!(
                "{NOT_REVIEWED}: the job was not saved by its owner; review it, then run \
                 `tole cron resume {id}` at a terminal"
            );
        }
        job.paused = false;
        job.at = now;
        l.put_job(job.clone())?;
        Ok(job)
    })
}
