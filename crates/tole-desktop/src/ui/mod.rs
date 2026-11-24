//! Renders the model. Colours, sizes and words come from [`tole_desktop_model::tokens`]
//! and [`tole_desktop_model::strings`]; `scripts/desktop-check.sh literals` warns about literals (advisory).
//!
//! The GPUI window is behind the `gpui` cargo feature (off by default, so a
//! plain `cargo test --workspace` does not build a GUI toolkit). Without it
//! [`run`] reports that the build has no window.
//!
//! The offscreen render harness (`harness`, feature `render-harness`) is not
//! part of any committed build: it needs a local patch that
//! `scripts/desktop-render.sh` applies to a temporary copy.

use std::fmt;

use tole_desktop_model::{strings, ShellState};

pub use self::status::{badge_text, Notice, Offer, Source, Status, LOOK_EVERY};
pub use self::task::TaskCall;

#[cfg(feature = "gpui")]
mod adapter;
#[cfg(feature = "render-harness")]
pub mod harness;
#[cfg(feature = "render-harness")]
pub mod harness_live;
#[cfg(feature = "render-harness")]
mod harness_scenes;
#[cfg(feature = "render-harness")]
pub mod harness_skills;
#[cfg(feature = "render-harness")]
pub mod harness_task;
#[cfg(feature = "gpui")]
mod header;
#[cfg(feature = "gpui")]
mod home;
#[cfg(feature = "gpui")]
mod notice;
#[cfg(feature = "gpui")]
mod shell;
#[cfg(feature = "gpui")]
mod sidebar;
#[cfg(feature = "gpui")]
mod skills;
mod status;
pub mod task;

/// Why [`run`] could not open a window.
#[derive(Debug)]
pub enum UiError {
    /// Built without the `gpui` feature.
    NoWindowToolkit,
    /// The toolkit could not open the window (no display, for example).
    Window(String),
}

impl fmt::Display for UiError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            UiError::NoWindowToolkit => f.write_str(strings::NO_WINDOW_TOOLKIT),
            UiError::Window(reason) => write!(f, "{}: {reason}", strings::WINDOW_FAILED),
        }
    }
}

impl std::error::Error for UiError {}

/// Opens the window for `state`, showing the app `start` builds, and blocks
/// until it closes.
///
/// `start` runs once, after the display is up and just as the window is built
/// (so a machine with no display never starts an app), on the window's thread.
/// An `Err` is the app that could not start: the window opens anyway and says
/// so, with the connection shown as unreachable. On a build without the
/// `gpui` feature `start` is not called.
#[cfg(feature = "gpui")]
pub fn run(
    state: ShellState,
    start: impl FnOnce() -> Result<Box<dyn Source>, String> + 'static,
) -> Result<(), UiError> {
    use tole_desktop_model::tokens::size;

    adapter::launch(
        strings::WINDOW_TITLE,
        size::WINDOW_WIDTH,
        size::WINDOW_HEIGHT,
        move |cx| match start() {
            Ok(source) => shell::ShellView::live(state, source, cx),
            Err(detail) => shell::ShellView::failed(state, Status::failed_to_start(detail)),
        },
    )
    .map_err(UiError::Window)
}

/// Opens the window for `state` and blocks until it closes. Nothing is behind
/// it: `state` is shown as it is (the sample data of a preview).
#[cfg(feature = "gpui")]
pub fn preview(state: ShellState) -> Result<(), UiError> {
    use tole_desktop_model::tokens::size;

    adapter::launch(
        strings::WINDOW_TITLE,
        size::WINDOW_WIDTH,
        size::WINDOW_HEIGHT,
        move |_cx| shell::ShellView::new(state),
    )
    .map_err(UiError::Window)
}

/// Without the toolkit there is no window, and `start` is not called.
#[cfg(not(feature = "gpui"))]
pub fn run(
    state: ShellState,
    start: impl FnOnce() -> Result<Box<dyn Source>, String> + 'static,
) -> Result<(), UiError> {
    let _ = (state, start);
    Err(UiError::NoWindowToolkit)
}

/// Opens the window for `state` and blocks until it closes.
#[cfg(not(feature = "gpui"))]
pub fn preview(state: ShellState) -> Result<(), UiError> {
    let _ = state;
    Err(UiError::NoWindowToolkit)
}

#[cfg(all(test, not(feature = "gpui")))]
mod tests {
    use super::*;

    #[test]
    fn a_preview_without_the_toolkit_says_so() {
        let err = preview(ShellState::placeholder()).unwrap_err();
        assert!(err.to_string().contains("--features gpui"));
    }

    #[test]
    fn a_run_without_the_toolkit_says_so_and_never_starts_the_app() {
        let err = run(ShellState::new(), || panic!("the app must not start")).unwrap_err();
        assert!(matches!(err, UiError::NoWindowToolkit));
    }
}
