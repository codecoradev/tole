//! Task adapter: typed `tole-client` values to the model's
//! [`TaskEvent`](tole_desktop_model::task::TaskEvent)s (#1031, part of #527).
//!
//! No toolkit, no network, no threads, no timers. The mapping table and the rules
//! it follows are the module doc of `task/event.rs` in `tole-desktop-model`; each
//! module here documents the decisions that table leaves open.
//!
//! - [`EntryMapper`]: durable entries (history and live).
//! - [`history_page`]: pages of `GET /entries`, and when the history is whole.
//! - [`stream_item`], [`stream_result`], [`status_ready`]: the live stream and
//!   `GET /status`.
//! - [`approval_requested`], [`approval_resolved`], [`approvals_refreshed`]: the
//!   approval rows.
//! - [`prompt_outcome`]: what the prompt call's return means.
//! - [`ResumePolling`], [`stop_step`]: the decisions after an allow and a Stop.
//!
//! **Text passes through raw**; the model makes it printable and cuts it.
//! **Never panics on server data**: values are read with `get` and `as_*`, a
//! missing or mistyped field maps to nothing or to the careful event, and
//! counts saturate. **Gaps** (what the typed client does not carry): a prompt
//! reply has no `seq`, so its `text` cannot become an `AnswerEntry`; `tool_started`
//! carries no intent id; an `Approval` row has no `risk`.

mod approval;
mod effects;
mod entry;
mod history;
mod live;
mod plan;
mod prompt;
#[cfg(test)]
mod tests;

pub use approval::{
    approval_info, approval_requested, approval_resolved, approvals_backoff, approvals_refreshed,
    reply_approval, APPROVALS_RETRY_BASE, APPROVALS_RETRY_MAX, MAX_APPROVALS_RETRIES,
    MAX_REFRESH_ROWS,
};
pub use effects::{
    stop_step, BusyPolling, BusyStep, ResumePolling, ResumeStep, StopStep, MAX_POLL_FAILURES,
    MAX_STOP_POLLS, RESUME_POLL, RESUME_TIMEOUT, SLOW_BUSY_POLL,
};
pub use entry::EntryMapper;
pub use history::{history_page, HistoryPage, PageStep, MAX_HISTORY_PAGES};
pub use live::{status_ready, stream_item, stream_result};
pub use prompt::{prompt_outcome, PromptOutcome};
