//! What the page asks of the app behind the window, as plain data (no toolkit).

use tole_desktop_model::approvals::{Decision, Tab};

/// What the owner did on the Approvals page. A press carries the id of the row
/// or the card that was pressed, never "whatever is selected".
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ApprovalsCall {
    /// A button of the card with this id. The model refuses it when the request
    /// is not waiting, is being answered or ran out of time, so a second press
    /// sends nothing.
    Decide {
        id: String,
        decision: Decision,
    },
    /// A row of the list was picked (`None`: nothing).
    Select(Option<String>),
    SelectTab(Tab),
}

/// Why a call did nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApprovalsRefused {
    /// There is no approvals driver for the serve that is there now (it is
    /// not ready, or the app ended): nothing was sent.
    NoServe,
    /// The driver's inbox is full or the driver stopped: nothing was sent.
    Busy,
}
