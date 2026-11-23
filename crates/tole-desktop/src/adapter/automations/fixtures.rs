//! Answers built from the server's own JSON, for the tests of the adapter and
//! of the automations driver.

use std::io;

use serde_json::{json, Value};
use tole_client::{ApprovalList, CronJobState, CronJobs, CronRuns, Error};

/// One job of `GET /cron/jobs`: due at 02:00 every night. `state` is `active`
/// or `paused`, `created_by` is `cli` or `agent`.
pub(crate) fn job_json(id: &str, created_by: &str, state: &str, review: bool) -> Value {
    let active = state == "active";
    json!({
        "id": id, "name": format!("job {id}"), "schedule": "0 2 * * *",
        "timezone": "UTC", "mode": "agent", "state": state,
        "next_run": active.then_some(1_700_000_000_i64),
        "next": if active { "2026-11-24 02:00 UTC" } else { "paused" },
        "last": null, "missed": 0, "blocked": 0,
        "created_by": created_by, "awaiting_review": review
    })
}

/// An answer of `GET /cron/jobs` with a scheduler that is alive in serve.
pub(crate) fn jobs_answer(jobs: Vec<Value>) -> CronJobs {
    serde_json::from_value(json!({
        "jobs": jobs,
        "scheduler": {"alive": true, "host": "serve", "age_seconds": 3}
    }))
    .expect("a cron jobs answer")
}

/// One run of `GET /cron/runs`.
pub(crate) fn run_json(
    fire: &str,
    job: &str,
    status: Option<&str>,
    session: Option<&str>,
) -> Value {
    json!({
        "fire_id": fire, "job_id": job, "scheduled_for": 1_700_000_000_i64,
        "status": status, "session_id": session, "usage": null,
        "deliveries": [], "output": "Moved 12 files", "output_truncated": false
    })
}

pub(crate) fn runs_answer(runs: Vec<Value>) -> CronRuns {
    serde_json::from_value(json!({ "runs": runs })).expect("a cron runs answer")
}

/// One row of `GET /approvals`.
pub(crate) fn approval_json(id: &str, session: &str, status: &str) -> Value {
    json!({"id": id, "tool": "bash", "status": status, "sessionId": session})
}

pub(crate) fn approvals_answer(rows: Vec<Value>) -> ApprovalList {
    serde_json::from_value(json!({ "approvals": rows })).expect("an approvals answer")
}

/// The answer to a pause or a resume.
pub(crate) fn job_state(id: &str, state: &str) -> CronJobState {
    serde_json::from_value(json!({"id": id, "state": state})).expect("a job state")
}

pub(crate) fn status_error(status: u16, message: &str) -> Error {
    Error::Status {
        status,
        message: message.into(),
        body: Value::Null,
    }
}

pub(crate) fn io_error() -> Error {
    Error::Io(io::Error::from(io::ErrorKind::ConnectionReset))
}
