//! Renders the model. Colours, sizes and words come from [`tole_desktop_model::tokens`]
//! and [`tole_desktop_model::strings`].
//!
//! The window itself needs a GUI toolkit, which is not part of this crate yet:
//! [`run`] reports that the build has no window.

use std::fmt;

use tole_desktop_model::{strings, ShellState};

/// Why [`run`] could not open a window.
#[derive(Debug)]
pub enum UiError {
    /// This build has no window toolkit.
    NoWindowToolkit,
}

impl fmt::Display for UiError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            UiError::NoWindowToolkit => f.write_str(strings::NO_WINDOW_TOOLKIT),
        }
    }
}

impl std::error::Error for UiError {}

/// Opens the window for `state` and blocks until it closes.
pub fn run(state: ShellState) -> Result<(), UiError> {
    let _ = state;
    Err(UiError::NoWindowToolkit)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn run_without_a_toolkit_says_so() {
        let err = run(ShellState::placeholder()).unwrap_err();
        assert_eq!(err.to_string(), strings::NO_WINDOW_TOOLKIT);
    }
}
