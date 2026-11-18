//! Webhook delivery of a cron run (#911, design section 4): the producer and
//! the host side of the sender. The pure and the file parts live in
//! `webhook_out` (the event builders, the outbox store, `drain_pass`); this
//! module is where they meet the cron ledger.
//!
//! - [`enqueue_run`]: the producer. It builds the `cron_run_finished` event,
//!   redacts it, and appends it to the outbox, then returns. It never does
//!   network I/O, so a dead receiver cannot slow a tick.
//! - [`record_settled`]: writes the terminal ledger `delivery` row for an
//!   event the sender settled. The two locks (the outbox's and `jobs.lock`)
//!   are never held together.
//! - [`drain`]: runs passes for a bounded time; the standalone `tole cron
//!   tick` and `tole cron run` call it so a delivery does not wait for a
//!   serve process.

use std::path::Path;
use std::time::{Duration, Instant};

use tole_core::ledger::{DeliveryStatus, Job, RunStatus};
use tole_core::secrets::SecretSet;

use super::deliver::Target;
use super::store;
use crate::webhook_out::event::{self, CronRun};
use crate::webhook_out::send::{self, EnvFileKeys, NetPoster, Settled};
use crate::webhook_out::store::{Enqueued, Outbox};
use crate::webhook_out::Destinations;

/// What the producer did with one `webhook:` target of a run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Produced {
    /// On the outbox: the sender writes the ledger row when it settles.
    Queued,
    /// A silent `ok` run: record `skipped`, never send.
    Skipped,
    /// Not queued: record `failed` with this code now.
    Failed(&'static str),
}

/// A settled run, as the producer needs it.
pub(crate) struct RunFacts<'a> {
    pub(crate) job: &'a Job,
    pub(crate) fire_id: &'a str,
    pub(crate) scheduled_for: i64,
    pub(crate) status: RunStatus,
    /// The run's output, already redacted once.
    pub(crate) output: &'a str,
    pub(crate) session_id: Option<&'a str>,
    /// When the run settled; stamps the event and the outbox line.
    pub(crate) finished_at: i64,
}

/// Queues the `cron_run_finished` event of a run for destination `name`.
///
/// Silent: a first line `[SILENT]` or empty output on an `ok` run delivers
/// nothing, but an `error`, `budget`, `cancelled`, `interrupted` or
/// `awaiting_approval` run with no output still notifies (an empty `summary`),
/// because a failure with no output is what an owner most wants to hear about.
pub(crate) fn enqueue_run(
    dir: &Path,
    dests: Option<&Destinations>,
    name: &str,
    run: &RunFacts<'_>,
    secrets: &SecretSet,
) -> Produced {
    if event::cron_is_silent(run.status, run.output) {
        return Produced::Skipped;
    }
    // Checked at the claim too; a log edited by hand, or a destination gone
    // since, must still never be sent to.
    let Some(d) = dests.and_then(|d| d.get(name)) else {
        return Produced::Failed("bad_destination");
    };
    if !d
        .events
        .contains(&crate::webhook_out::Event::CronRunFinished)
    {
        return Produced::Failed("bad_destination");
    }
    let facts = CronRun {
        job_id: &run.job.id,
        job_name: super::field(run.job, "name"),
        fire_id: run.fire_id,
        status: run.status,
        scheduled_for: run.scheduled_for,
        finished_at: run.finished_at,
        output: run.output,
        session_id: run.session_id,
    };
    let redact = |s: &str| secrets.redact(s).into_owned();
    let Ok(built) = event::cron_run_finished(name, run.finished_at, &facts, &redact) else {
        return Produced::Failed("enqueue_failed");
    };
    let Ok(body) = String::from_utf8(built.body) else {
        return Produced::Failed("enqueue_failed");
    };
    match Outbox::new(dir).enqueue(name, &built.id, &body, run.finished_at) {
        Ok(Enqueued::Queued | Enqueued::Duplicate) => Produced::Queued,
        Err(e) => Produced::Failed(e.code()),
    }
}

/// Writes the terminal ledger row of each settled event that belongs to a run.
/// `Settled` is advisory: a row is written only when `webhook:<dest>` is
/// really a target of that fire's job, so a line the file made up cannot put a
/// row on a run it does not belong to. Returns how many rows were written.
pub fn record_settled(dir: &Path, rows: Vec<Settled>, now: i64) -> anyhow::Result<usize> {
    let rows: Vec<Settled> = rows.into_iter().filter(|r| r.fire_id.is_some()).collect();
    if rows.is_empty() {
        return Ok(0);
    }
    store::with_lock(dir, |l| {
        let mut written = 0;
        for r in rows {
            let Some(fire) = r.fire_id.as_deref() else {
                continue;
            };
            let Some(job_id) = l.state().done(fire).map(|d| d.job_id.clone()) else {
                continue;
            };
            let names_it = l
                .state()
                .job(&job_id)
                .and_then(|j| super::deliver::targets(&j.job).ok())
                .is_some_and(|ts| ts.contains(&Target::Webhook(r.dest.clone())));
            if !names_it {
                continue;
            }
            let target = format!("webhook:{}", r.dest);
            // One terminal row per (run, destination). A delivery row is always
            // terminal, so an existing one means a slow drainer, a repeated
            // pass or a later reconcile already recorded it.
            if l.state().deliveries(fire).any(|d| d.target == target) {
                continue;
            }
            if l.deliver(fire, &target, r.status, r.code, now).is_ok() {
                written += 1;
            }
        }
        Ok(written)
    })
}

/// Runs drain passes for about `budget` (a soft bound: a send in flight is
/// not cut, so a pass may run up to a destination's timeout, at most 30 s,
/// longer; no new pass starts when less than the longest timeout is left). It
/// goes on while a pass sent something, while the outbox lock was busy, and
/// while a retry of a configured destination falls due inside what is left
/// (the first waits are 2 s and 10 s), checking once a second; a longer wait is
/// for the next drain. Ledger rows are written even when a pass fails midway.
pub fn drain(
    dir: &Path,
    dests: &Destinations,
    budget: Duration,
) -> anyhow::Result<send::PassStats> {
    let started = Instant::now();
    let mut total = send::PassStats::default();
    let outbox = Outbox::new(dir);
    let longest = dests
        .names()
        .filter_map(|n| dests.get(n))
        .map(|d| d.timeout)
        .max()
        .unwrap_or_default();
    loop {
        let mut rows = Vec::new();
        let pass = send::drain_pass(
            &outbox,
            dests,
            &NetPoster,
            &EnvFileKeys,
            &super::now,
            &mut |s| rows.push(s),
        );
        // The rows of what this pass already settled are recorded first, so an
        // error after some events were sent does not leave them without one.
        let wrote = record_settled(dir, rows, super::now());
        let stats = pass?;
        wrote?;
        total.sent += stats.sent;
        total.retried += stats.retried;
        total.dead += stats.dead;
        let left = budget.saturating_sub(started.elapsed());
        if left < longest.min(budget) || left.is_zero() {
            return Ok(total);
        }
        let worked = stats.sent + stats.dead > 0;
        if worked {
            continue; // more may be due: no wait between events
        }
        if !stats.busy && stats.retried == 0 && !pending_within(&outbox, dests, left) {
            return Ok(total);
        }
        total.busy |= stats.busy;
        std::thread::sleep(Duration::from_secs(1).min(left));
    }
}

/// Whether an event of a destination that is configured falls due within
/// `window`: a retry whose wait is short, or a lease about to run out. An
/// event whose destination is gone is never sent, so it never counts (it would
/// make every drain wait out the whole budget).
fn pending_within(outbox: &Outbox, dests: &Destinations, window: Duration) -> bool {
    let now = super::now();
    let Ok(fold) = outbox.fold(crate::webhook_out::outbox::FoldOptions {
        now,
        max_lease_ahead: 1000,
    }) else {
        return false;
    };
    let horizon = now.saturating_add(window.as_secs() as i64);
    let due = fold.entries().any(|(dest, _, e)| {
        dests.get(dest).is_some()
            && e.pending()
            && e.deadline > now
            && e.next_at.max(e.lease_until) <= horizon
    });
    due
}

/// What a skipped or failed outcome records on the ledger.
pub(crate) fn immediate(p: &Produced) -> Option<(DeliveryStatus, Option<String>)> {
    match p {
        Produced::Queued => None,
        Produced::Skipped => Some((DeliveryStatus::Skipped, None)),
        Produced::Failed(code) => Some((DeliveryStatus::Failed, Some((*code).to_string()))),
    }
}
