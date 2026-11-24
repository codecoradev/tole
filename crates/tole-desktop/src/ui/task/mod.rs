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
//! - `rows`, `prose`: one row of each kind, an answer's Markdown.

mod call;
#[cfg(feature = "gpui")]
mod list;
pub mod measure;
#[cfg(feature = "gpui")]
mod prose;
#[cfg(feature = "gpui")]
mod rows;
#[cfg(feature = "gpui")]
mod state;

pub use self::call::TaskCall;
#[cfg(feature = "gpui")]
pub(super) use self::state::TaskPanes;
