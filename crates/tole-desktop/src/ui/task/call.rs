//! What the window asks of an open task.

/// One thing the owner or the list did in a task. The source passes it to the
/// task's driver by session id (`Backend`). `Viewport`, `Heights` and `Toggle`
/// are the list's own bookkeeping; `Send`, `Steer` and `Stop` are the owner's
/// commands, and the driver may still refuse them (see `TaskView::ignored`).
#[derive(Debug, Clone, PartialEq)]
pub enum TaskCall {
    /// The list shows the rows around `top` (pixels), `height` tall, with
    /// `overscan` more each side (`Handle::set_viewport`).
    Viewport {
        top: f32,
        height: f32,
        overscan: f32,
    },
    /// The heights of rows the list built (`Handle::set_heights`).
    Heights(Vec<(usize, f32)>),
    /// Open or close the "Working" group at this row.
    Toggle(usize),
    /// A new prompt (`SendMode::Send`).
    Send(String),
    /// A note to the running turn (`SendMode::Steer`).
    Steer(String),
    /// Stop the running turn.
    Stop,
}
