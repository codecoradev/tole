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
/// How often the serve sender reconciles the ledger and the outbox.
const RECONCILE_EVERY_SECS: i64 = 300;
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
        let mut next_reconcile = 0;
        loop {
            // At start (a crash recovery) and every 5 minutes: close the gaps
            // a crash leaves between the ledger and the outbox.
            if super::now() >= next_reconcile {
                next_reconcile = super::now() + RECONCILE_EVERY_SECS;
                let secrets =
                    crate::provider_chain::process_secrets(&crate::config_apply::real_env);
                // A panic here must not end delivery either.
                let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    reconcile(&dir, &dests, super::now(), &secrets)
                }));
                match r {
                    Ok(Ok(r)) if r.enqueued + r.rows > 0 => eprintln!(
                        "tole serve: webhook reconcile queued {} event(s), wrote {} row(s)",
                        r.enqueued, r.rows
                    ),
                    Ok(Ok(_)) => {}
                    Ok(Err(e)) => eprintln!("tole serve: webhook reconcile: {e}"),
                    Err(_) => eprintln!("tole serve: webhook reconcile hit an internal error"),
                }
            }
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
            // The target parser already requires a valid name; checked again
            // here because the saved jobs are untrusted input and these names
            // are printed by `tole doctor`.
            Target::Webhook(n) if crate::webhook_out::valid_name(&n) => Some(n),
            _ => None,
        })
        .collect()
}

/// What one reconcile pass did.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Reconciled {
    /// Events queued again for a run that had none.
    pub enqueued: usize,
    /// Ledger rows written (a skipped or failed run, or the row of an event
    /// that finished without one).
    pub rows: usize,
}

/// A run older than this is not announced any more: it is past the deadline an
/// event would have had. `tole cron runs` still shows it.
const RECONCILE_WINDOW_SECS: i64 = 6 * 3600;
/// A run younger than this is left to the live tick: its event is queued by
/// the tick that ran it, after its other targets were delivered (a command
/// target can take minutes), and a reconcile pass in that window would race it.
const RECONCILE_GRACE_SECS: i64 = 600;
/// How much of a run's kept output reconcile reads: the event's summary is cut
/// far below this.
const OUTPUT_HEAD_BYTES: u64 = 64 * 1024;
/// Most events one pass queues, so a long outage cannot make a pass unbounded.
const RECONCILE_MAX_ENQUEUE: usize = 100;

/// The reconcile pass (#911, design section 4): closes the two gaps a crash
/// leaves between the ledger and the outbox.
///
/// 1. A run settled but its event was never queued (the process died between
///    the `done` record and the append): for each run that settled in the last
///    6 h whose job names a configured destination, with neither an outbox
///    event for it nor a ledger row of any status, queue it (or record
///    `skipped` for a silent run, `failed` for a refusal). A row of ANY status
///    counts as terminal: otherwise silent runs would be queued again on every
///    pass and notify against the silent rule.
/// 2. An event reached `done` or `dead` but its ledger row was never written
///    (the process died between the two): write the row from the outbox's
///    terminal line.
///
/// It first COLLECTS what is missing from the ledger and the outbox, holding
/// no lock, and only then calls `enqueue` (which re-checks atomically under the
/// outbox lock, so the pass is idempotent) and writes rows (under the ledger
/// lock). The two locks are never held together.
pub fn reconcile(
    dir: &Path,
    dests: &Destinations,
    now: i64,
    secrets: &SecretSet,
) -> anyhow::Result<Reconciled> {
    let mut done = Reconciled::default();
    if dests.is_empty() {
        return Ok(done);
    }
    let outbox = Outbox::new(dir);
    let events = outbox.fold(crate::webhook_out::outbox::FoldOptions {
        now,
        max_lease_ahead: 0,
    })?;

    // 1. Collect the runs that have neither an event nor a row.
    struct Missing {
        name: String,
        fire_id: String,
        job: Job,
        scheduled_for: i64,
        status: RunStatus,
        session_id: Option<String>,
        finished_at: i64,
    }
    let mut missing = Vec::new();
    {
        let l = store::open_read(dir)?;
        let st = l.state();
        for d in st.dones() {
            let age = now.saturating_sub(d.at);
            if !(RECONCILE_GRACE_SECS..=RECONCILE_WINDOW_SECS).contains(&age) {
                continue;
            }
            let Some(job) = st.job(&d.job_id).map(|j| j.job.clone()) else {
                continue;
            };
            if job.removed {
                continue;
            }
            for t in super::deliver::targets(&job).unwrap_or_default() {
                let Target::Webhook(name) = t else { continue };
                if dests.get(&name).is_none() {
                    continue;
                }
                let target = format!("webhook:{name}");
                let has_row = st.deliveries(&d.fire_id).any(|x| x.target == target);
                let id = event::cron_event_id(&d.fire_id);
                if has_row || events.has(&name, &id) {
                    continue;
                }
                missing.push(Missing {
                    name,
                    fire_id: d.fire_id.clone(),
                    job: job.clone(),
                    scheduled_for: st.fire(&d.fire_id).map_or(d.at, |f| f.scheduled_for),
                    status: d.status,
                    session_id: d.session_id.clone(),
                    finished_at: d.at,
                });
                if missing.len() >= RECONCILE_MAX_ENQUEUE {
                    break;
                }
            }
            if missing.len() >= RECONCILE_MAX_ENQUEUE {
                break;
            }
        }
    }

    // Queue them (no ledger lock held), then record what could not be queued.
    let mut rows = Vec::new();
    for m in missing {
        // The run's output was kept, redacted, when it settled, and the live
        // tick writes the file (even an empty one) before it settles the run. A
        // file that is not there or cannot be read means this run never went
        // through the live path (settled at claim time, or recovered after a
        // crash) or the write failed: leave it. Never guess "silent" from a
        // missing file, which would record `skipped` for a message the live
        // tick may still send.
        let Some(output) = read_output_head(&super::tick::output_path(dir, &m.fire_id)) else {
            continue;
        };
        let facts = RunFacts {
            job: &m.job,
            fire_id: &m.fire_id,
            scheduled_for: m.scheduled_for,
            status: m.status,
            output: &output,
            session_id: m.session_id.as_deref(),
            finished_at: m.finished_at,
        };
        let made = enqueue_run(dir, Some(dests), &m.name, &facts, secrets);
        match &made {
            Produced::Queued => done.enqueued += 1,
            other => {
                if let Some((status, error)) = immediate(other) {
                    rows.push((m.fire_id, format!("webhook:{}", m.name), status, error));
                }
            }
        }
    }
    if !rows.is_empty() {
        done.rows += store::with_lock(dir, |l| {
            let mut n = 0;
            for (fire, target, status, error) in rows {
                // Once per run and destination, whoever wrote it first.
                if l.state().deliveries(&fire).any(|d| d.target == target) {
                    continue;
                }
                if l.deliver(&fire, &target, status, error, now).is_ok() {
                    n += 1;
                }
            }
            Ok(n)
        })?;
    }

    // 2. An event that finished without a ledger row. Only events of the window
    // are looked at, and the ledger lock is only taken when a row is missing.
    let settled: Vec<Settled> = outbox
        .terminal_events(now, RECONCILE_WINDOW_SECS)?
        .into_iter()
        .filter(|t| t.fire_id.is_some())
        .map(|t| {
            let (status, code) = match t.terminal {
                crate::webhook_out::outbox::Terminal::Done => (DeliveryStatus::Sent, None),
                crate::webhook_out::outbox::Terminal::Dead(c) => {
                    (DeliveryStatus::Failed, Some(known_code(&c).to_string()))
                }
            };
            Settled {
                dest: t.dest,
                id: t.id,
                fire_id: t.fire_id,
                status,
                code,
            }
        })
        .collect();
    let settled: Vec<Settled> = {
        let l = store::open_read(dir)?;
        settled
            .into_iter()
            .filter(|s| {
                let target = format!("webhook:{}", s.dest);
                s.fire_id
                    .as_deref()
                    .is_some_and(|f| !l.state().deliveries(f).any(|d| d.target == target))
            })
            .collect()
    };
    done.rows += record_settled(dir, settled, now)?;
    Ok(done)
}

/// The first [`OUTPUT_HEAD_BYTES`] of a run's kept output, or `None` when it is
/// not there or not a plain file. The path is inside the cron dir, which the
/// same account can write: the file is opened without following a symlink and
/// without blocking (a planted FIFO), must be a regular file, and is read up to
/// a cap, so a planted link cannot put another file in a webhook body and a
/// huge or special file cannot stall the sender.
fn read_output_head(path: &Path) -> Option<String> {
    use std::io::Read;
    let mut o = std::fs::OpenOptions::new();
    o.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        o.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    let f = o.open(path).ok()?;
    if !f.metadata().ok()?.is_file() {
        return None;
    }
    let mut bytes = Vec::new();
    f.take(OUTPUT_HEAD_BYTES).read_to_end(&mut bytes).ok()?;
    Some(String::from_utf8_lossy(&bytes).into_owned())
}
