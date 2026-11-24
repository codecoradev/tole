//! The task view (board S3, #1031): the transcript of an open task.
//!
//! What it shows and when comes from the model (`tole_desktop_model::task`) and
//! from the driver's [`TaskView`](crate::driver::task::TaskView): the rows of the
//! window, their padding, the status and the marks. This module draws them and
//! reports what the toolkit laid out, and decides nothing the model decides.
//!
//! - `measure`: the numbers the list learns from its last frame (no toolkit).
//! - `call`: what the window asks of a task (no toolkit).
//! - `list`: the scrolling column of rows, following the end of a running task.
//! - `strip`: the tabs of the open tasks and the status row; `compose`: the
//!   composer; `outbox`: when a message that went out must come back (no toolkit).
//! - `rows`, `work`, `prose`: one row of each kind, a "Working" group, an
//!   answer's Markdown.

mod call;
#[cfg(all(feature = "gpui", unix))]
mod compose;
#[cfg(all(feature = "gpui", unix))]
mod list;
pub mod measure;
pub mod outbox;
#[cfg(all(feature = "gpui", unix))]
mod prose;
#[cfg(all(feature = "gpui", unix))]
mod rows;
#[cfg(all(feature = "gpui", unix))]
mod state;
#[cfg(all(feature = "gpui", unix))]
mod strip;
#[cfg(all(feature = "gpui", unix))]
mod work;

pub use self::call::TaskCall;
#[cfg(all(feature = "gpui", unix))]
pub(super) use self::state::TaskPanes;

// The task view reads the driver's `TaskView`, and there is no driver where
// there is no serve supervisor (not macOS or Linux): the window has no task
// pane there.
#[cfg(all(feature = "gpui", not(unix)))]
mod none;
