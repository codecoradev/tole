//! The Automations page (board S6, issue #1033): the model, without a window.
//!
//! Input is the model's own mirror of the serve routes of #1042
//! (`GET /cron/jobs`, `GET /cron/runs`, `POST /cron/jobs/{id}/pause` and
//! `/resume`). The adapter from the `tole-client` types is a later step:
//!
//! - `CronJobs.jobs[i]` (`CronJob`) maps field for field to [`RawJob`], with
//!   `folder: None` (the serve row has no working folder yet);
//!   `CronJobs.scheduler` gives `Scheduler::new(alive, host)`.
//! - `CronRuns.runs[i]` (`CronRun`) maps to [`RawRun`] (`status`,
//!   `session_id`, `output`, `output_truncated`); `approval_id` is the id of
//!   the pending approval with that `session_id` in the Approvals list.
//! - A `CronJobState` becomes `ToggleResult::Done { state }`; a client error
//!   with an HTTP status becomes `Refused { status, message }`.

mod job;
mod run;
mod schedule;
mod state;
mod toggle;

pub use job::{AutomationId, AutomationRow, NextRun, Origin, RawJob, RowNote, Switch};
pub use run::{RawRun, RunEntry, RunOutcome};
pub use schedule::Schedule;
pub use state::{AutomationsState, Scheduler, SchedulerHost};
pub use toggle::{Notice, ToggleIntent, ToggleRequest, ToggleResult, OWNER_RESUME_COMMAND};
