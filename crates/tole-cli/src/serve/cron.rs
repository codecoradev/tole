//! Cron over serve (#1042): `GET /cron/jobs`, `POST /cron/jobs/{id}/pause` and
//! `POST /cron/jobs/{id}/resume`, next to the `GET /cron/runs` history.
//!
//! A thin face over `tole_cli::cron` ([`tole_cli::cron::view`]); the scheduler
//! itself is the in-process ticker of `tole serve --cron` or `tole cron tick`,
//! and `scheduler` in the list says whether one is alive. Creating, editing and
//! removing a job are local-only owner actions, and resuming a job the owner has
//! not reviewed is refused (`403`): that review is trust approval.

use serde_json::{json, Value};
use tole_cli::cron::{self, view};

use super::State;

/// `GET /cron/jobs`.
pub(super) fn list() -> (u16, Value) {
    match cron::default_dir().and_then(|dir| view::jobs_json(&dir, cron::now())) {
        Ok(v) => (200, v),
        Err(e) => (500, json!({"error": format!("{e:#}")})),
    }
}

/// `POST /cron/jobs/{id}/pause` and `/resume`: the job by its id, never by name.
/// A `--plan-mode` server pauses but does not resume: resuming gives a job
/// authority to run again.
pub(super) fn set_paused(state: &State, id: &str, paused: bool) -> (u16, Value) {
    if !paused && state.plan_mode {
        return (
            409,
            json!({"error": "this server runs in plan mode; it pauses jobs but does not resume them"}),
        );
    }
    let done = cron::default_dir().and_then(|dir| {
        if paused {
            view::pause(&dir, id, cron::now())
        } else {
            view::resume(&dir, id, cron::now())
        }
    });
    match done {
        Ok(job) => (
            200,
            json!({"id": job.id, "state": if job.paused { "paused" } else { "active" }}),
        ),
        Err(e) => {
            let msg = format!("{e:#}");
            let status = if msg.starts_with("no job") {
                404
            } else if msg.starts_with(view::NOT_REVIEWED) {
                403
            } else {
                500
            };
            (status, json!({"error": msg}))
        }
    }
}
