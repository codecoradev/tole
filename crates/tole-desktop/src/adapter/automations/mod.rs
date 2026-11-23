//! Automations: `GET /cron/jobs`, `GET /cron/runs` and the answers to a pause
//! or a resume, to the model's `AutomationsState` (board S6, #1033).
//!
//! The mapping is the one the model's `automations` module documents:
//!
//! - A `CronJob` is a [`RawJob`](tole_desktop_model::RawJob) ([`raw_job`]),
//!   `CronJobs.scheduler` a `Scheduler` ([`scheduler`]). `mode` and
//!   `scheduler.age_seconds` have no field in the model and are not mapped.
//! - A `CronRun` is a [`RawRun`](tole_desktop_model::RawRun) ([`raw_run`]); the
//!   run row has no approval, so the link of a run that stopped to ask
//!   ("Open in Approvals") is looked up here: the first approval that is
//!   still `pending` and belongs to the run's session ([`runs`]). `job_id`,
//!   `usage` and `deliveries` have no field and are not mapped.
//! - A `CronJobState` or an error of a pause or resume is a `ToggleResult`
//!   ([`toggle_result`]).
//!
//! What the schedule in words, the "next run" text and the on/off switch of a
//! row are is the model's: it works them out from the raw fields, and a word it
//! does not know (a state, an origin, a status) reads as the careful value (off,
//! not the owner's, not a pass). Nothing is computed twice here.
//!
//! # What the client does not carry
//!
//! - **The working folder.** `CronJob` has no folder (the serve row does not
//!   send one), so every row has `folder: None` and the table's folder column
//!   stays empty until the route sends it.
//! - **The notifications card** (configured gateways, retry window, send a
//!   test). The client has no call for it, and serve has no route: there is
//!   nothing for an adapter to map. It needs a route first.
//! - **A partial flag.** Neither answer says "this is not all of it". The
//!   adapter's own caps are the only partial reads there are, and they are
//!   reported next to the rows ([`Jobs::left_out`], [`Runs::more_at_least`]).
//!
//! # Text
//!
//! The model cleans every string it draws, so text goes through here as the
//! server sent it, except that each field is cut at a bound far above what the
//! model draws (1,024 characters, 4,096 for a run's output). A cut ends in an
//! ellipsis, so a cut word never reads as a known one. The one exception is the
//! job's id: it is what pause and resume send back, so it is never cut or
//! changed. A job whose id is longer than [`MAX_ID_BYTES`] (a request line for
//! it could not be sent) is left out and counted, so the ids the page holds
//! are bounded too. The ids of runs are not sent in any request, and are
//! bounded like any other text.
//!
//! # Bounds
//!
//! At most [`MAX_JOBS`] jobs and [`MAX_RUNS`] runs are kept, the first ones, in
//! the server's order. What was left out is a count next to the rows, never a
//! silent cut: the page says "N more not shown" and does not present its
//! numbers as the whole. A job whose id an earlier row has, or whose id is too
//! long, is left out too.
//! Runs of another job than the one asked for are dropped.
//!
//! # Late replies
//!
//! This module has no counter: which reply is the latest of a route is the
//! driver's to know (it has the calls and the clock), and
//! `driver::automations` does not apply a reply it no longer trusts.

mod apply;
#[cfg(test)]
mod bounds_tests;
#[cfg(test)]
pub(crate) mod fixtures;
mod jobs;
mod runs;
#[cfg(test)]
mod tests;
mod text;
mod toggle;

pub use apply::{apply_jobs, apply_runs, JobsApplied, RunsApplied};
pub use jobs::{jobs, raw_job, scheduler, Jobs, MAX_ID_BYTES, MAX_JOBS};
pub use runs::{needs_approval_lookup, raw_run, runs, Runs, MAX_RUNS, RUNS_LIMIT};
pub use toggle::toggle_result;
