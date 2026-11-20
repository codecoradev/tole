//! The `tole-desktop` binary: opens the window, or says why it cannot.

use std::process::ExitCode;

use tole_desktop::ui;
use tole_desktop_model::ShellState;

fn main() -> ExitCode {
    match ui::run(ShellState::placeholder()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("tole-desktop: {err}");
            ExitCode::FAILURE
        }
    }
}
