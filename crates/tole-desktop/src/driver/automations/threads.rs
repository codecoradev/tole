//! The bodies of the driver's I/O threads. None decides anything: they block on
//! a call and report what came back to the worker as an [`Input`].
//!
//! Each ends when the stop signal is raised, or when the worker is gone (its
//! inbox, or the channel it feeds this thread through, is closed).
//!
//! A panic never goes unnoticed. A panic inside an `Api` call is caught where
//! the call is made and becomes the failure of that call, so the thread goes on
//! and the engine retries a read (a switch is never retried: its failure is
//! shown and the list is read again). A panic anywhere else in a thread is
//! caught by `run_guarded` (`driver::shared::thread`), which tells the worker
//! (`ThreadFailed`); the worker then ends the driver, which leaves the view not
//! loaded.

use std::sync::mpsc::{Receiver, SyncSender};
use std::sync::Arc;

use tole_desktop_model::{AutomationId, ToggleIntent, ToggleRequest};

use super::engine::{CallId, Input, RunsReply};
use crate::adapter::automations::{needs_approval_lookup, RUNS_LIMIT};
use crate::driver::api::Api;
use crate::driver::clock::Shutdown;
use crate::driver::guard::{guarded, panicked};

/// Runs the `GET /cron/jobs` calls the engine asks for, one at a time. The
/// answer is sent whatever it is (a full inbox waits: an answer is never
/// dropped).
pub(super) fn jobs_loader(
    api: Arc<dyn Api>,
    jobs: Receiver<CallId>,
    inbox: SyncSender<Input>,
    stop: Shutdown,
) {
    for id in jobs {
        if stop.is_set() {
            return;
        }
        let reply = guarded(|| api.cron_jobs()).unwrap_or_else(|| Err(panicked()));
        if inbox.send(Input::Jobs(id, reply)).is_err() {
            return;
        }
    }
}

/// Runs the `GET /cron/runs` calls the engine asks for, one at a time. When a
/// run of the answer stopped to ask the owner, the pending approvals are read
/// too (`GET /approvals`), for the link of that run; a failure of that second
/// call does not fail the first (the adapter says the links are not known).
pub(super) fn runs_loader(
    api: Arc<dyn Api>,
    jobs: Receiver<(CallId, AutomationId)>,
    inbox: SyncSender<Input>,
    stop: Shutdown,
) {
    for (id, job) in jobs {
        if stop.is_set() {
            return;
        }
        let runs =
            guarded(|| api.cron_runs(job.as_str(), RUNS_LIMIT)).unwrap_or_else(|| Err(panicked()));
        let lookup = match &runs {
            Ok(list) if needs_approval_lookup(job.as_str(), list) => {
                Some(guarded(|| api.approvals(None)).unwrap_or_else(|| Err(panicked())))
            }
            _ => None,
        };
        if inbox
            .send(Input::Runs(id, RunsReply { job, runs, lookup }))
            .is_err()
        {
            return;
        }
    }
}

/// Sends the pauses and resumes the engine asks for, one at a time, each once:
/// whatever comes back, a call is never repeated here.
pub(super) fn toggler(
    api: Arc<dyn Api>,
    jobs: Receiver<(CallId, ToggleRequest)>,
    inbox: SyncSender<Input>,
    stop: Shutdown,
) {
    for (id, request) in jobs {
        if stop.is_set() {
            return;
        }
        let job = request.id.as_str();
        let result = guarded(|| match request.intent {
            ToggleIntent::Pause => api.pause_cron_job(job),
            ToggleIntent::Resume => api.resume_cron_job(job),
        })
        .unwrap_or_else(|| Err(panicked()));
        if inbox.send(Input::Toggled(id, request, result)).is_err() {
            return;
        }
    }
}
