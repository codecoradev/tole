//! Renders the model. Colours, sizes and words come from [`tole_desktop_model::tokens`]
//! and [`tole_desktop_model::strings`]; `scripts/desktop-check.sh literals` warns about literals (advisory).
//!
//! The GPUI window is behind the `gpui` cargo feature (off by default, so a
//! plain `cargo test --workspace` does not build a GUI toolkit). Without it
//! [`run`] reports that the build has no window.

use std::fmt;

use tole_desktop_model::{strings, ShellState};

#[cfg(feature = "gpui")]
mod adapter;
#[cfg(feature = "gpui")]
mod shell;

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

/// Opens the window for `state` and blocks until it closes.
#[cfg(feature = "gpui")]
pub fn run(state: ShellState) -> Result<(), UiError> {
    use tole_desktop_model::tokens::size;

    adapter::launch(
        strings::WINDOW_TITLE,
        size::WINDOW_WIDTH,
        size::WINDOW_HEIGHT,
        move |_cx| shell::ShellView::new(state),
    )
    .map_err(UiError::Window)
}

/// Opens the window for `state` and blocks until it closes.
#[cfg(not(feature = "gpui"))]
pub fn run(state: ShellState) -> Result<(), UiError> {
    let _ = state;
    Err(UiError::NoWindowToolkit)
}

#[cfg(all(test, not(feature = "gpui")))]
mod tests {
    use super::*;

    #[test]
    fn run_without_the_toolkit_says_so() {
        let err = run(ShellState::placeholder()).unwrap_err();
        assert!(err.to_string().contains("--features gpui"));
    }
}
