//! What the window asks of an open task.

/// One thing the owner or the list did in a task. The source passes it to the
/// task's driver by session id (`Backend`); the first three are the list's own
/// bookkeeping, not commands: they change no state of the task.
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
}
