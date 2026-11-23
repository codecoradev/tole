//! `GET /cron/runs` to the model's [`RawRun`] rows, with the link to the
//! approval of a run that stopped to ask.

use std::collections::HashMap;

use tole_client::{ApprovalList, CronRun, CronRuns};
use tole_desktop_model::RawRun;

use super::text::{bounded, cut, OUTPUT_MAX, TEXT_MAX};

/// Most runs of one automation kept.
pub const MAX_RUNS: usize = 100;
/// The `limit` to ask for: one more than is kept, so a full answer says there
/// may be older runs.
pub const RUNS_LIMIT: usize = MAX_RUNS + 1;

/// The status of a run that stopped to ask the owner.
const STOPPED_TO_ASK: &str = "awaiting_approval";

/// The history of one automation as the model takes it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Runs {
    /// Newest first, as the server sends them, at most [`MAX_RUNS`].
    pub rows: Vec<RawRun>,
    /// Runs of the answer that are not in `rows`. The server may hold more
    /// that it did not send (it is asked for [`RUNS_LIMIT`]), so this is a
    /// lower bound: draw "at least N older runs not shown".
    pub more_at_least: usize,
    /// Whether every run that stopped to ask has the answer to "is its
    /// approval still waiting?": true when no run needs it or the approval
    /// list was read. When false a run without a link means "not known", not
    /// "nothing is waiting".
    pub links_known: bool,
}

/// The runs of `job` in the answer, newest first. A run the answer gives for
/// another job is no part of this history and is dropped (a server that
/// ignores the `job` filter must not put another automation's runs here).
fn of_job<'a>(job: &'a str, list: &'a CronRuns) -> impl Iterator<Item = &'a CronRun> {
    list.runs.iter().filter(move |r| r.job_id == job)
}

/// A run that stopped to ask and says which session it parked in: the only
/// kind that can have a link to an approval.
fn needs_link(run: &CronRun) -> bool {
    run.status.as_deref() == Some(STOPPED_TO_ASK) && run.session_id.is_some()
}

/// Whether the history of `job` has a run whose approval link needs the
/// pending approvals (`GET /approvals`): the driver makes that call only then.
pub fn needs_approval_lookup(job: &str, list: &CronRuns) -> bool {
    of_job(job, list).take(MAX_RUNS).any(needs_link)
}

/// The id of the first pending approval of each session.
fn pending_by_session(list: &ApprovalList) -> HashMap<&str, &str> {
    let mut map = HashMap::new();
    for row in list.approvals.iter().filter(|a| a.status == "pending") {
        if let Some(session) = row.session_id.as_deref() {
            map.entry(session).or_insert(row.id.as_str());
        }
    }
    map
}

/// A `CronRun` as the model's run. `approval_id` is the lookup the caller made
/// (the serve row does not carry it). `output_truncated` is the server's flag,
/// also raised when this adapter cut the text; a status the server sent is
/// passed on as it is (the model reads an unknown word as "not a pass").
pub fn raw_run(run: &CronRun, approval_id: Option<&str>) -> RawRun {
    let (output, cut_here) = cut(&run.output, OUTPUT_MAX);
    RawRun {
        fire_id: bounded(&run.fire_id, TEXT_MAX),
        scheduled_for: run.scheduled_for,
        status: run.status.as_deref().map(|s| bounded(s, TEXT_MAX)),
        session_id: run.session_id.as_deref().map(|s| bounded(s, TEXT_MAX)),
        approval_id: approval_id.map(|s| bounded(s, TEXT_MAX)),
        output,
        output_truncated: run.output_truncated || cut_here,
    }
}

/// The history of `job` from an answer of `Client::cron_runs`, bounded, with
/// the link of each run that stopped to ask: the first approval of
/// `pending` that is still `pending` and belongs to the run's session. `pending`
/// is `None` when the list was not read; a run that needs it then has no link
/// and `links_known` is false.
pub fn runs(job: &str, list: &CronRuns, pending: Option<&ApprovalList>) -> Runs {
    let lookup = pending.map(pending_by_session);
    let mut all = of_job(job, list);
    let kept: Vec<&CronRun> = all.by_ref().take(MAX_RUNS).collect();
    let more_at_least = all.count();
    let links_known = lookup.is_some() || !kept.iter().any(|r| needs_link(r));
    let rows = kept
        .into_iter()
        .map(|run| {
            let link = lookup
                .as_ref()
                .filter(|_| needs_link(run))
                .and_then(|m| m.get(run.session_id.as_deref()?).copied());
            raw_run(run, link)
        })
        .collect();
    Runs {
        rows,
        more_at_least,
        links_known,
    }
}
