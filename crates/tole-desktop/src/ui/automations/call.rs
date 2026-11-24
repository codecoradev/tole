//! What the page asks of the app behind the window, as plain data (no toolkit).

/// What the owner did on the Automations page. A press carries the id of the
/// row that was pressed, never "whatever is selected".
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AutomationsCall {
    /// The switch of the row with this id: turn it on (`true`) or off.
    SetEnabled { id: String, enabled: bool },
    /// A row was picked: show what happened on its runs.
    Select(String),
    /// The notice under the table was dismissed.
    DismissNotice,
}

/// Why a call did nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AutomationsRefused {
    /// There is no automations driver for the serve that is there now (it is
    /// not ready, or the app ended): nothing was sent.
    NoServe,
    /// The driver's inbox is full or the driver stopped: nothing was sent.
    Busy,
}
