//! The view of a model state with no driver behind it: for a window that only
//! previews (the render harness, a test of the page), never for the real app.

use tole_desktop_model::approvals::ApprovalsState;

use super::view::ApprovalsView;

impl ApprovalsView {
    /// `state` as the page reads it at `now_ms`, with `fetch_failures` failed
    /// fetches in a row and the given parked sessions. The same derivation as
    /// the driver's own view, so a preview shows what the driver would.
    pub fn preview(
        state: &ApprovalsState,
        now_ms: u64,
        fetch_failures: u32,
        parked_sessions: Vec<String>,
    ) -> Self {
        Self::of(state, now_ms, fetch_failures, parked_sessions)
    }
}
