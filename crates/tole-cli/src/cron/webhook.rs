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

/// How often the `tole serve` sender looks at the outbox (one `stat`).
const SENDER_POLL: Duration = Duration::from_secs(5);
/// Most passes one look runs back to back: a pass sends one event per
/// destination, so a backlog is drained in a burst, bounded so a flood cannot
/// keep one look busy forever.
const MAX_PASSES_PER_LOOK: usize = 200;

/// The `tole serve` sender (#911): one long-lived thread that drains the
/// outbox. It notices new lines by the file's length (a `stat` every 5 s), runs
/// passes when the file grew or a retry is due, writes the ledger rows of what
/// it settles, and logs one stderr line per failed delivery with the
/// destination NAME and a closed code only. It never joins on exit: pending
/// events are on disk and an attempt in flight is abandoned (the receiver may
/// have seen it, so it is sent again next start).
///
/// Started whether or not `--cron` is given: a webhook route can run a job
/// without the ticker, and the approval events (a later part) need it too.
pub fn spawn_sender(dir: std::path::PathBuf, dests: std::sync::Arc<Destinations>) {
    std::thread::spawn(move || {
        let mut sender = Sender::new(&dir);
        loop {
            // A panic must not silently end delivery for the life of serve.
            let look = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                sender.step(&dests, super::now(), &mut |dest, code| {
                    eprintln!("tole serve: webhook delivery to `{dest}` failed: {code}");
                })
            }));
            if look.is_err() {
                eprintln!("tole serve: the webhook sender hit an internal error; continuing");
            }
            std::thread::sleep(SENDER_POLL);
        }
    });
}

/// The state of the sender loop, so a step is a plain function of its inputs.
pub struct Sender {
    dir: std::path::PathBuf,
    outbox: Outbox,
    /// The file length after the last look; a different length means new lines.
    seen_len: Option<u64>,
    /// A pass is due at this time even if the file did not grow (a retry, a
    /// busy lock, or a backoff after an error).
    wake_at: i64,
    /// Consecutive failed looks, and the last message (logged once).
    errors: u32,
    last_error: String,
}

impl Sender {
    pub fn new(dir: &Path) -> Sender {
        Sender {
            dir: dir.to_path_buf(),
            outbox: Outbox::new(dir),
            seen_len: None,
            wake_at: 0,
            errors: 0,
            last_error: String::new(),
        }
    }

    /// One look at the outbox. Runs passes when the file changed or a wake time
    /// came, until nothing more is due (a backlog drains in a burst, not one
    /// event per look). `failed` is called for each delivery that ended
    /// `failed`, with the destination name and its code. Returns whether a pass
    /// ran.
    pub fn step(
        &mut self,
        dests: &Destinations,
        now: i64,
        failed: &mut dyn FnMut(&str, &str),
    ) -> bool {
        use crate::webhook_out::outbox::{MAX_RETRY_WAIT_SECS, SKEW_SECS};
        let changed = self.seen_len != Some(self.outbox.size());
        // A wake time far beyond what any wait can be means the clock stepped
        // back since it was set: treat it as due.
        let due = self.wake_at != 0
            && (now >= self.wake_at || self.wake_at > now + MAX_RETRY_WAIT_SECS + SKEW_SECS + 5);
        if !changed && !due {
            return false;
        }
        let mut outcome = Ok(send::PassStats::default());
        for _ in 0..MAX_PASSES_PER_LOOK {
            outcome = self.one_pass(dests, failed);
            match &outcome {
                Ok(s) if s.sent + s.dead > 0 && !s.busy => continue,
                _ => break,
            }
        }
        match outcome {
            Ok(stats) => {
                self.errors = 0;
                self.last_error.clear();
                self.seen_len = Some(self.outbox.size());
                // Come back for a retry (or a busy lock) when it falls due,
                // not only when the file grows. A fold that failed is retried
                // soon rather than forgotten.
                self.wake_at = if stats.busy {
                    now + SENDER_POLL.as_secs() as i64
                } else {
                    match next_wake(&self.outbox, dests, now) {
                        Ok(w) => w.unwrap_or(0),
                        Err(()) => now + SENDER_POLL.as_secs() as i64,
                    }
                };
            }
            Err(e) => {
                // Back off (5 s doubling to 5 min) instead of retrying every
                // look, and say it once: a full disk or an oversized file
                // would otherwise log and re-read the file forever.
                self.errors = self.errors.saturating_add(1);
                if e != self.last_error {
                    eprintln!("tole serve: webhook send: {e}");
                    self.last_error = e;
                }
                let wait = (5i64 << self.errors.min(6)).min(300);
                self.seen_len = Some(self.outbox.size());
                self.wake_at = now + wait;
            }
        }
        true
    }

    /// One drain pass, recording the rows of what it settled.
    fn one_pass(
        &mut self,
        dests: &Destinations,
        failed: &mut dyn FnMut(&str, &str),
    ) -> Result<send::PassStats, String> {
        let mut rows = Vec::new();
        let pass = send::drain_pass(
            &self.outbox,
            dests,
            &NetPoster,
            &EnvFileKeys,
            &super::now,
            &mut |s| rows.push(s),
        );
        for r in &rows {
            // A destination name from the file is only printed when it is a
            // valid name: the file is untrusted and a log is not a place for
            // control characters.
            if r.status == DeliveryStatus::Failed && crate::webhook_out::valid_name(&r.dest) {
                failed(&r.dest, known_code(r.code.as_deref().unwrap_or("")));
            }
        }
        // Rows first, even when the pass failed midway (see `drain`). A row
        // that cannot be written is reported; the reconcile pass (the next
        // part) closes that gap from the outbox's terminal line.
        if let Err(e) = record_settled(&self.dir, rows, super::now()) {
            eprintln!("tole serve: webhook ledger rows not written: {e}");
        }
        pass.map_err(|e| e.to_string())
    }
}

/// A result code from the closed set, else `other`: a code read from the file
/// is untrusted and is printed in a log and in `tole doctor`.
fn known_code(code: &str) -> &str {
    const FIXED: [&str; 10] = [
        "timeout",
        "dns",
        "refused_address",
        "redirect_refused",
        "bad_destination",
        "expired",
        "bad_line",
        "key_unavailable",
        "enqueue_failed",
        "queue_full",
    ];
    let http = code
        .strip_prefix("http_")
        .is_some_and(|n| n.len() == 3 && n.bytes().all(|b| b.is_ascii_digit()));
    if http || FIXED.contains(&code) {
        code
    } else {
        "other"
    }
}

/// When the earliest pending event of a configured destination is next due
/// (its retry time or lease end, at least a second from now). `Ok(None)` when
/// nothing is pending for a configured destination; `Err` when the outbox could
/// not be read (the caller looks again soon).
fn next_wake(outbox: &Outbox, dests: &Destinations, now: i64) -> Result<Option<i64>, ()> {
    let fold = outbox
        .fold(crate::webhook_out::outbox::FoldOptions {
            now,
            max_lease_ahead: 1000,
        })
        .map_err(|_| ())?;
    let next = fold
        .entries()
        .filter(|(dest, _, e)| dests.get(dest).is_some() && e.pending())
        .map(|(_, _, e)| e.next_at.max(e.lease_until).max(now + 1))
        .min();
    Ok(next)
}

/// `tole doctor` lines for the outbox (#911): pending and failed counts, the
/// age of the oldest pending event, the file's size against its cap, and jobs
/// whose destination is gone. Counts and names only, never a URL, a body or a
/// secret. The bool is false when something needs fixing now.
pub fn doctor_lines(dir: &Path, dests: &Destinations, now: i64) -> (Vec<String>, bool) {
    use crate::webhook_out::outbox::{Terminal, MAX_FILE_BYTES, WARN_FILE_BYTES};
    let outbox = Outbox::new(dir);
    let mut lines = Vec::new();
    let mut ok = true;
    let size = outbox.size();
    let names_a_destination = webhook_targets(dir);
    if size == 0 && names_a_destination.is_empty() && dests.is_empty() {
        return (lines, ok);
    }
    if size >= MAX_FILE_BYTES {
        ok = false;
        lines.push(format!(
            "  ! webhook outbox is {} MiB, at its cap: new events are refused (queue_full); move outbox.jsonl aside when nothing is pending",
            size / (1024 * 1024)
        ));
    } else if size >= WARN_FILE_BYTES {
        lines.push(format!(
            "  ~ webhook outbox is {} MiB (cap {} MiB); move outbox.jsonl aside when nothing is pending",
            size / (1024 * 1024),
            MAX_FILE_BYTES / (1024 * 1024)
        ));
    }
    if let Ok(fold) = outbox.fold(crate::webhook_out::outbox::FoldOptions {
        now,
        max_lease_ahead: 1000,
    }) {
        let mut pending = 0;
        let mut oldest = i64::MAX;
        let (mut failed, mut last_code) = (0, String::new());
        for (_, _, e) in fold.entries() {
            if e.pending() {
                pending += 1;
                oldest = oldest.min(e.at);
            } else if let Some(Terminal::Dead(code)) = &e.terminal {
                if now.saturating_sub(e.at) <= 24 * 3600 {
                    failed += 1;
                    last_code = code.clone();
                }
            }
        }
        if pending > 0 {
            let age = now.saturating_sub(oldest);
            let mark = if age > 600 { "  ~" } else { " " };
            lines.push(format!(
                "{mark} webhook outbox: {pending} pending (oldest {}s)",
                age.max(0)
            ));
        }
        if failed > 0 {
            lines.push(format!(
                "  ~ webhook events given up in the last 24 h: {failed} (last: {})",
                known_code(&last_code)
            ));
        }
        if fold.bad_lines > 0 {
            lines.push(format!(
                "  ~ webhook outbox: {} line(s) skipped as unreadable",
                fold.bad_lines
            ));
        }
    }
    for name in names_a_destination {
        if dests.get(&name).is_none() {
            ok = false;
            lines.push(format!(
                "  ! a job delivers to `webhook:{name}`, which is not configured (it is blocked: declare [webhook_out.{name}])"
            ));
        }
    }
    (lines, ok)
}

/// The names of the destinations the saved jobs deliver to.
fn webhook_targets(dir: &Path) -> std::collections::BTreeSet<String> {
    let Ok(l) = store::open_read(dir) else {
        return Default::default();
    };
    l.state()
        .jobs()
        // A paused job cannot run, so a missing destination is not yet a
        // problem: `deliver::check` blocks it on resume.
        .filter(|j| !j.job.removed && !j.job.paused)
        .filter_map(|j| super::deliver::targets(&j.job).ok())
        .flatten()
        .filter_map(|t| match t {
            Target::Webhook(n) => Some(n),
            _ => None,
        })
        .collect()
}
